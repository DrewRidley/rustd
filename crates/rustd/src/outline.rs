//! Telling an edit to the bodies of functions from one the rest of the program can see.
//!
//! Most edits change what a function does and nothing else. What was compiled from other
//! bodies is then as it was, and only what comes from the edited file needs another look. That
//! holds for a body nothing else sees into: not that of a `const fn`, which other items are
//! evaluated with, nor one whose type is part of its signature (`async fn`, `-> impl Trait`),
//! nor one a macro is handed, nor one that declares items other than statics and constants.
//! Such bodies count as part of what the rest of the program sees.

use std::hash::{Hash, Hasher};

use hir_def::{FunctionId, Lookup, ModuleDefId, expr_store::Body};
use ide_db::{
    RootDatabase,
    base_db::{EditionedFileId, relevant_crates},
};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use syntax::{
    AstNode, Edition, SyntaxKind, SyntaxNode, TextRange, WalkEvent,
    ast::{self, HasAttrs},
};
use vfs::FileId;

use crate::Listed;

/// Attributes that leave the item they are on as it is written.
const INERT: &[&str] = &[
    "allow",
    "cfg",
    "cold",
    "deny",
    "deprecated",
    "doc",
    "expect",
    "forbid",
    "inline",
    "must_use",
    "track_caller",
    "warn",
];

/// Each file as it was when last looked at.
#[derive(Default)]
pub(crate) struct Outlines {
    files: FxHashMap<FileId, Option<u64>>,
}

impl Outlines {
    /// Takes note of a file that is about to change, unless it is known already.
    pub(crate) fn before_edit(&mut self, db: &RootDatabase, listed: &Listed, file: FileId) {
        self.files.entry(file).or_insert_with(|| outline(db, listed, file));
    }

    /// For each file of the edit that was just applied, whether nothing changed in it but
    /// bodies that nothing else sees into. Asked after [`Outlines::before_edit`].
    pub(crate) fn after_edit(
        &mut self,
        db: &RootDatabase,
        listed: &Listed,
        files: Vec<FileId>,
    ) -> Vec<(FileId, bool)> {
        let off = std::env::var_os("RUSTD_NO_BODIES").is_some();
        let files: Vec<_> = files
            .into_iter()
            .map(|file| {
                let now = outline(db, listed, file);
                let was = self.files.insert(file, now);
                (file, !off && now.is_some() && was == Some(now))
            })
            .collect();
        // An edit to more than bodies can change which functions there are, in any file, and
        // what is kept here was found by asking the ones listed before it.
        if files.iter().any(|&(_, bodies_only)| !bodies_only) {
            self.files.clear();
        }
        files
    }
}

/// A hash of everything in the file but the bodies nothing else sees into, or `None` when the
/// file is not all ours to compile or is not read as a file of its own.
fn outline(db: &RootDatabase, listed: &Listed, file: FileId) -> Option<u64> {
    let stats = std::env::var_os("RUSTD_STATS").is_some();
    let Some(functions) = listed.in_file(db, file) else {
        if stats {
            eprintln!("outline: {file:?} has functions that are not listed");
        }
        return None;
    };
    // A function the edit took away is an edit to more than bodies, and asking for its body
    // panics.
    let seen = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::lower(db, functions.iter().map(|&it| it.into()).collect());
        Written::of(db, &functions)
    }));
    let seen = match seen {
        Ok(Some(seen)) => seen,
        Ok(None) => {
            if stats {
                eprintln!("outline: {file:?} is included in another file");
            }
            return None;
        }
        Err(_) => {
            if stats {
                eprintln!("outline: {file:?}: a listed function is gone");
            }
            return None;
        }
    };
    // As the crate reads it, so that the file is parsed once for both.
    let edition =
        relevant_crates(db, file).first().map_or(Edition::CURRENT, |it| it.data(db).edition);
    let tree = EditionedFileId::new(db, file, edition).parse(db).syntax_node();
    let (hash, found) = hash_outside_bodies(&tree, &seen.declaring);
    // Each of them has to be one of the bodies that were kept in.
    (found == seen.declaring.len()).then_some(hash)
}

/// What looking at the functions written in a file tells that how they are written does not.
struct Written {
    /// Where the functions are whose bodies declare items.
    declaring: FxHashSet<TextRange>,
}

impl Written {
    /// `None` when the functions are in a file that `include!` reads: those are written by a
    /// macro as far as we can tell.
    fn of(db: &RootDatabase, functions: &[FunctionId]) -> Option<Written> {
        let mut declaring = FxHashSet::default();
        for &func in functions {
            let at = func.lookup(db).id;
            if at.file_id.is_macro() {
                let included = at.file_id.original_file_respecting_includes(db)
                    != at.file_id.original_file(db);
                if included {
                    return None;
                }
                // What a macro wrote comes from what it was handed, which is no body that
                // is left out.
                continue;
            }
            if declares_items(db, func) {
                declaring.insert(at.to_range(db));
            }
        }
        Some(Written { declaring })
    }
}

fn declares_items(db: &RootDatabase, func: FunctionId) -> bool {
    Body::of(db, func.into()).blocks(db).any(|(_, def_map)| {
        def_map.modules(db).any(|(_, module)| {
            let scope = module.scope(db);
            // A static or constant is the body's own: only its code names it, and neither is
            // a function to list. Macros that log declare them.
            scope
                .declarations()
                .any(|it| !matches!(it, ModuleDefId::StaticId(_) | ModuleDefId::ConstId(_)))
                || scope.impls().len() != 0
                || scope.unnamed_consts().next().is_some()
        })
    })
}

/// A hash of everything in `text` except function bodies that nothing outside the function can
/// see into. Used by the Clippy workspace wrapper to tell when an edit to an upstream crate is
/// inside function bodies alone and cannot change what downstream crates check.
pub(crate) fn syntax_outline_hash(text: &str) -> u64 {
    let tree = syntax::SourceFile::parse(text, Edition::CURRENT).syntax_node();
    let mut declaring = FxHashSet::default();
    for node in tree.descendants() {
        if node.kind() == SyntaxKind::FN
            && let Some(func) = ast::Fn::cast(node.clone())
            && let Some(body) = func.body()
        {
            let has_items_or_macros = body.syntax().descendants().skip(1).any(|it| {
                matches!(
                    it.kind(),
                    SyntaxKind::FN
                        | SyntaxKind::STRUCT
                        | SyntaxKind::ENUM
                        | SyntaxKind::UNION
                        | SyntaxKind::TRAIT
                        | SyntaxKind::IMPL
                        | SyntaxKind::TYPE_ALIAS
                        | SyntaxKind::MODULE
                        | SyntaxKind::EXTERN_BLOCK
                        | SyntaxKind::MACRO_RULES
                        | SyntaxKind::MACRO_DEF
                        | SyntaxKind::MACRO_CALL
                )
            });
            if has_items_or_macros {
                declaring.insert(node.text_range());
            }
        }
    }
    hash_outside_bodies(&tree, &declaring).0
}

/// The hash, and how many of the `declaring` functions were come by.
fn hash_outside_bodies(file: &SyntaxNode, declaring: &FxHashSet<TextRange>) -> (u64, usize) {
    let mut hasher = FxHasher::default();
    let mut found = 0;
    let mut walk = file.preorder_with_tokens();
    while let Some(event) = walk.next() {
        match event {
            WalkEvent::Enter(syntax::NodeOrToken::Node(node)) => {
                node.kind().hash(&mut hasher);
                if node.kind() == SyntaxKind::FN && declaring.contains(&node.text_range()) {
                    found += 1;
                } else if is_unseen_body(&node)
                    && node.parent().is_none_or(|it| !declaring.contains(&it.text_range()))
                {
                    walk.skip_subtree();
                }
            }
            WalkEvent::Enter(syntax::NodeOrToken::Token(token)) => {
                // Blank space and comments say nothing, except to a macro that is handed them.
                // A doc comment is a token of another kind.
                let blank = matches!(token.kind(), SyntaxKind::WHITESPACE | SyntaxKind::COMMENT)
                    && token.parent().is_none_or(|it| it.kind() != SyntaxKind::TOKEN_TREE);
                if !blank {
                    token.kind().hash(&mut hasher);
                    token.text().hash(&mut hasher);
                }
            }
            WalkEvent::Leave(_) => {}
        }
    }
    (hasher.finish(), found)
}

/// Whether the node is the body of a function that nothing else sees into, going by how the
/// function is written. What its body declares takes name resolution to tell.
fn is_unseen_body(node: &SyntaxNode) -> bool {
    if node.kind() != SyntaxKind::BLOCK_EXPR {
        return false;
    }
    let Some(func) = node.parent().and_then(ast::Fn::cast) else { return false };
    if func.const_token().is_some() || func.async_token().is_some() || func.gen_token().is_some() {
        return false;
    }
    let names_its_type = func.ret_type().is_some_and(|it| {
        it.syntax().descendants().any(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE)
    });
    if names_its_type || !is_inert(func.syntax()) {
        return false;
    }
    // rustc copies the body into the callers, also the ones it compiled for us.
    let always_inlined = func.attrs().any(|attr| {
        attr.simple_name().is_some_and(|name| name == "inline")
            && attr.syntax().text().to_string().contains("always")
    });
    if always_inlined {
        return false;
    }
    // An attribute written inside the body is the function's, and a macro declared in it can
    // be exported from the crate.
    let reaches_out = node.descendants().any(|it| match it.kind() {
        SyntaxKind::ATTR => it.ancestors().nth(2).is_some_and(|block| block == *node),
        SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => true,
        _ => false,
    });
    if reaches_out {
        return false;
    }
    // Only items, all the way up: a function in a body is that body's, and one in a macro's
    // input is the macro's.
    func.syntax().ancestors().skip(1).all(|it| match it.kind() {
        SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::ASSOC_ITEM_LIST => true,
        SyntaxKind::MODULE | SyntaxKind::IMPL | SyntaxKind::TRAIT => is_inert(&it),
        _ => false,
    })
}

/// Whether no attribute on the item can be a macro that rewrites it.
fn is_inert(item: &SyntaxNode) -> bool {
    let Some(item) = ast::AnyHasAttrs::cast(item.clone()) else { return false };
    item.attrs().all(|attr| attr.simple_name().is_some_and(|name| INERT.contains(&name.as_str())))
}

impl Listed {
    /// Whether the file is a module of a crate that is built, or of one that such a crate uses.
    /// Any file may be one before a crate is listed.
    pub(crate) fn sees(&self, db: &RootDatabase, file: FileId) -> bool {
        // Copied out: a query that is cancelled unwinds, and would poison the lock.
        let built = self.built.lock().unwrap().clone();
        // Before the first build there is no telling.
        built.is_empty()
            || relevant_crates(db, file).iter().any(|&krate| {
                let read = built.iter().any(|&built| {
                    let built = ide_db::base_db::Crate::from(built);
                    built == krate || built.transitive_deps(db).contains(&krate)
                });
                read && hir_def::nameres::crate_def_map(db, krate)
                    .modules_for_file(db, file)
                    .next()
                    .is_some()
            })
    }

    /// The functions written in the file, or `None` when it has some that are not listed: it
    /// belongs to a crate that the ones being built use and that is not one of them.
    pub(crate) fn in_file(&self, db: &RootDatabase, file: FileId) -> Option<Vec<FunctionId>> {
        let mut functions = Vec::new();
        let listed = self.functions.lock().unwrap().clone();
        for &krate in relevant_crates(db, file).iter() {
            match listed.get(&krate.into()) {
                Some(listed) => functions.extend(
                    listed
                        .functions
                        .iter()
                        .copied()
                        .filter(|&it| hir_ty::mir::codegen::body_file(db, it.into()) == file),
                ),
                None if listed.keys().any(|&built| {
                    ide_db::base_db::Crate::from(built).transitive_deps(db).contains(&krate)
                }) =>
                {
                    return None;
                }
                // Nothing that is built can see it.
                None => {}
            }
        }
        Some(functions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntax::SourceFile;

    fn hash(text: &str) -> u64 {
        let tree = SourceFile::parse(text, Edition::CURRENT).syntax_node();
        hash_outside_bodies(&tree, &FxHashSet::default()).0
    }

    #[track_caller]
    fn unseen(before: &str, after: &str) {
        assert_eq!(hash(before), hash(after), "{before:?} to {after:?} is seen");
    }

    #[track_caller]
    fn seen(before: &str, after: &str) {
        assert_ne!(hash(before), hash(after), "{before:?} to {after:?} is not seen");
    }

    #[test]
    fn bodies_are_left_out() {
        unseen("fn f() { 1; }", "fn f() { 2; }");
        unseen("impl S { fn f(&self) -> u8 { 1 } }", "impl S { fn f(&self) -> u8 { 2 } }");
        unseen("trait T { fn f(&self) -> u8 { 1 } }", "trait T { fn f(&self) -> u8 { 2 } }");
        unseen("mod m { pub fn f() { a() } }", "mod m { pub fn f() { b() } }");
        unseen("fn f() { #[allow(unused)] let a = 1; }", "fn f() { #[allow(unused)] let a = 2; }");
        unseen("/// Doc.\n#[inline]\nfn f() { 1; }", "/// Doc.\n#[inline]\nfn f() { 2; }");
    }

    #[test]
    fn blank_space_and_comments_are_left_out() {
        unseen("fn f() {}", "\n\nfn f() {}\n");
        unseen("fn f() {}", "fn f() {}\n// edit\n");
        unseen("struct S;", "/* note */ struct S;");
    }

    #[test]
    fn what_others_see_is_kept() {
        seen("fn f() {}", "fn g() {}");
        seen("fn f() {}", "fn f() {} fn g() {}");
        seen("fn f() -> u8 { 1 }", "fn f() -> u16 { 1 }");
        seen("struct S(u8);", "struct S(u16);");
        seen("/// One.\nfn f() {}", "/// Two.\nfn f() {}");
        seen("const C: u8 = 1;", "const C: u8 = 2;");
        seen("static S: u8 = { 1 };", "static S: u8 = { 2 };");
    }

    #[test]
    fn bodies_others_see_into_are_kept() {
        seen("const fn f() -> u8 { 1 }", "const fn f() -> u8 { 2 }");
        seen("async fn f() -> u8 { 1 }", "async fn f() -> u8 { 2 }");
        seen("fn f() -> impl Sized { 1u8 }", "fn f() -> impl Sized { 1u16 }");
        seen("fn f() -> Box<impl Sized> { a() }", "fn f() -> Box<impl Sized> { b() }");
        seen("#[test]\nfn f() { 1; }", "#[test]\nfn f() { 2; }");
        seen("#[inline(always)]\nfn f() { 1; }", "#[inline(always)]\nfn f() { 2; }");
        seen("#[tokio::main]\nfn f() { 1; }", "#[tokio::main]\nfn f() { 2; }");
        seen("#[imp]\nimpl S { fn f() { 1; } }", "#[imp]\nimpl S { fn f() { 2; } }");
        seen("fn f() { #![attr] 1; }", "fn f() { #![attr] 2; }");
        seen(
            "fn f() { macro_rules! m { () => { 1 } } }",
            "fn f() { macro_rules! m { () => { 2 } } }",
        );
    }

    #[test]
    fn what_a_macro_is_handed_is_kept() {
        seen("m! { fn f() { 1; } }", "m! { fn f() { 2; } }");
        seen("m! { a b }", "m! { a  b }");
        seen("m! { a }", "m! { a /* note */ }");
        seen("fn f() { fn g() { 1; } }", "fn f() { fn g() { 1; } } fn h() {}");
    }

    #[test]
    fn a_body_that_declares_items_is_kept() {
        let text = |n: u8| format!("fn f() {{ fn g() -> u8 {{ {n} }} }}");
        let hash = |text: &str| {
            let tree = SourceFile::parse(text, Edition::CURRENT).syntax_node();
            let declaring = tree.descendants().find(|it| it.kind() == SyntaxKind::FN).unwrap();
            hash_outside_bodies(&tree, &FxHashSet::from_iter([declaring.text_range()]))
        };
        let (one, found) = hash(&text(1));
        assert_eq!(found, 1);
        assert_ne!(one, hash(&text(2)).0);
    }
}
