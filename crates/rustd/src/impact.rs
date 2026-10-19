//! Which tests of a test program an edit can change the outcome of.
//!
//! The client says which source files differ from when the tests last all passed, with
//! their text of then. A file in which more than function bodies changed, or one we can't
//! place, has every test that can see its crate run. Otherwise the functions whose bodies
//! changed are found, and a test is picked when it reaches one: through the functions it
//! calls or names, the impls of the traits it calls methods of, the impls of the types its
//! values are of (an operator, a drop or a `?` runs those unnamed), and the statics and
//! constants it reads. Anything this can't tell runs. `CARGO.md` has the rules.

use std::path::{Path, PathBuf};

use hir::{Crate, HasSource as _, db::HirDatabase as _};
use hir_def::{AdtId, DefWithBodyId, FunctionId, HasModule, ImplId, ItemContainerId, Lookup};
use hir_ty::mir::codegen::{body_deps, body_file, field_adts, impl_header_adts};
use ide_db::{RootDatabase, base_db::SourceDatabase as _};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use syntax::{
    AstNode, Edition, NodeOrToken, SourceFile, SyntaxKind, SyntaxNode, TextRange, WalkEvent,
    ast::{self, HasAttrs},
};

use crate::{Args, Watched, WatchedFile};

/// A source file that is not what it was when the tests last all passed.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Change {
    pub(crate) path: PathBuf,
    /// Its text then; `None` when it was not there.
    pub(crate) was: Option<String>,
    /// A hash of its text now, as [`text_hash`] has it; `None` when it is gone.
    pub(crate) now: Option<String>,
}

/// What of a test program to run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) enum Picked {
    /// All of it, and why.
    Whole(String),
    /// None of it, and why.
    Nothing(String),
    /// The tests of `run`, each with the edited function it reaches. A test the harness
    /// lists that is not in `known` was made by a macro, and is run too.
    Tests { known: Vec<String>, run: Vec<(String, String)> },
}

/// The hash a [`Change`] names a text by.
pub(crate) fn text_hash(text: &str) -> String {
    Sha256::digest(text).iter().map(|it| format!("{it:02x}")).collect()
}

/// What of the program `args` names to run after `changes`.
pub(crate) fn select(
    db: &RootDatabase,
    args: &Args,
    files: &[WatchedFile],
    changes: &[Change],
) -> Picked {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hir::attach_db(db, || pick(db, args, files, changes))
    }));
    caught
        .unwrap_or_else(|_| Picked::Whole("telling which tests an edit reaches failed".to_owned()))
}

fn pick(db: &RootDatabase, args: &Args, files: &[WatchedFile], changes: &[Change]) -> Picked {
    let own = match crate::crates_named(db, &args.krate, args) {
        Ok(own) => own,
        Err(why) => return Picked::Whole(format!("{why:#}")),
    };
    // Every crate the program is compiled from.
    let mut seen: FxHashSet<Crate> = own.iter().copied().collect();
    for krate in &own {
        let base = ide_db::base_db::Crate::from(*krate);
        seen.extend(base.transitive_deps(db).into_iter().map(Crate::from));
    }
    let edited = match edited_functions(db, files, changes, &seen) {
        Ok(edited) => edited,
        Err(why) => return Picked::Whole(why),
    };
    let tests = tests_of(db, &own);
    let roots: Vec<(String, DefWithBodyId)> = if tests.is_empty() {
        // A test target without the harness is a program: what its `main` reaches is it.
        let main = own
            .iter()
            .flat_map(|it| crate::functions_of(db, *it))
            .find(|&it| hir::Function::from(it).is_main(db));
        match main {
            Some(main) => vec![("main".to_owned(), main.into())],
            None => return Picked::Nothing("it has no tests".to_owned()),
        }
    } else {
        tests.iter().map(|(name, func)| (name.clone(), (*func).into())).collect()
    };
    if edited.is_empty() {
        return Picked::Nothing("no function it is compiled from was edited".to_owned());
    }
    let graph = Graph::new(db, &seen);
    let reaching = graph.reaching(roots.iter().map(|it| Node::Body(it.1)), &edited);
    if tests.is_empty() {
        return match reaching.get(&Node::Body(roots[0].1)) {
            Some(why) => Picked::Whole(format!("its `main` reaches {why}")),
            None => Picked::Nothing("its `main` reaches no edited function".to_owned()),
        };
    }
    let run = roots
        .iter()
        .filter_map(|(name, body)| Some((name.clone(), reaching.get(&Node::Body(*body))?.clone())))
        .collect();
    Picked::Tests { known: tests.into_iter().map(|it| it.0).collect(), run }
}

/// The functions whose bodies changed in the files of `changes` that the crates of `seen`
/// are compiled from, each with how it is shown. `Err` is why every test runs.
fn edited_functions(
    db: &RootDatabase,
    files: &[WatchedFile],
    changes: &[Change],
    seen: &FxHashSet<Crate>,
) -> Result<FxHashMap<DefWithBodyId, String>, String> {
    let ids: FxHashMap<&Path, vfs::FileId> = files
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some((&*it.path, id)),
            Watched::Included(_) => None,
        })
        .collect();
    let mut edited = FxHashMap::default();
    for change in changes {
        let shown = change.path.display();
        let Some(&file) = ids.get(&*change.path) else {
            return Err(format!("{shown} changed, and the daemon has not loaded it"));
        };
        let crates: Vec<Crate> = ide_db::base_db::relevant_crates(db, file)
            .iter()
            .map(|&it| Crate::from(it))
            .filter(|it| seen.contains(it))
            .collect();
        if crates.is_empty() {
            continue;
        }
        let text = db.file_text(file).text(db);
        if change.now.as_deref() != Some(&text_hash(text)) {
            return Err(format!("{shown} is not what the daemon has of it"));
        }
        let proc_macro = |it: &Crate| ide_db::base_db::Crate::from(*it).data(db).is_proc_macro;
        if let Some(krate) = crates.iter().find(|it| !it.origin(db).is_local() || proc_macro(it)) {
            let name = krate.display_name(db).map(|it| it.to_string()).unwrap_or_default();
            return Err(format!("{shown} changed, of `{name}`, whose code no test calls"));
        }
        let Some(was) = &change.was else {
            return Err(format!("{shown} is new"));
        };
        let edition = crates[0].edition(db);
        let ranges = edited_bodies(was, text, edition)
            .ok_or_else(|| format!("{shown}: more than the bodies of functions changed"))?;
        for range in ranges {
            let found = functions_at(db, &crates, file, range);
            if found.is_empty() {
                return Err(format!("{shown}: an edited function is not one the daemon knows"));
            }
            for func in found {
                let name = hir::Function::from(func).name(db).as_str().to_owned();
                edited.insert(func.into(), format!("`{name}` ({shown})"));
            }
        }
    }
    Ok(edited)
}

/// The functions of `crates` written in `file` whose source, or the macro call that wrote
/// them, overlaps `range`.
fn functions_at(
    db: &RootDatabase,
    crates: &[Crate],
    file: vfs::FileId,
    range: TextRange,
) -> Vec<FunctionId> {
    let mut found = Vec::new();
    for &krate in crates {
        for func in crate::functions_of(db, krate) {
            if body_file(db, func.into()) != file {
                continue;
            }
            let Some(source) = hir::Function::from(func).source(db) else { continue };
            let at = source.as_ref().map(|it| it.syntax()).original_file_range_rooted(db);
            if at.file_id.file_id(db) == file && at.range.intersect(range).is_some() {
                found.push(func);
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// The tests of the program's crates, by the names the harness gives them.
fn tests_of(db: &RootDatabase, crates: &[Crate]) -> Vec<(String, FunctionId)> {
    let mut tests = Vec::new();
    for &krate in crates {
        for id in crate::functions_of(db, krate) {
            let function = hir::Function::from(id);
            if !function.is_test(db) && !function.is_bench(db) {
                continue;
            }
            let module = function.module(db);
            let mut path: Vec<String> = module
                .path_to_root(db)
                .into_iter()
                .rev()
                .filter_map(|it| Some(it.name(db)?.as_str().to_owned()))
                .collect();
            path.push(function.name(db).as_str().to_owned());
            tests.push((path.join("::"), id));
        }
    }
    tests.sort();
    tests.dedup();
    tests
}

/// Where in `now` the functions are whose bodies are all that differ from `was`, or `None`
/// when more than such bodies changed.
fn edited_bodies(was: &str, now: &str, edition: Edition) -> Option<Vec<TextRange>> {
    let was = Outline::of(&SourceFile::parse(was, edition).syntax_node());
    let now = Outline::of(&SourceFile::parse(now, edition).syntax_node());
    if was.rest != now.rest || was.bodies.len() != now.bodies.len() {
        return None;
    }
    let edited = std::iter::zip(&was.bodies, &now.bodies).filter(|(was, now)| was.1 != now.1);
    Some(edited.map(|(_, now)| now.0).collect())
}

/// A file as the tests can tell it: the text of each body that only calls reach, by where
/// its function is, and the rest of the file.
struct Outline {
    bodies: Vec<(TextRange, String)>,
    rest: String,
}

impl Outline {
    fn of(file: &SyntaxNode) -> Outline {
        let mut outline = Outline { bodies: Vec::new(), rest: String::new() };
        let mut walk = file.preorder_with_tokens();
        while let Some(event) = walk.next() {
            match event {
                WalkEvent::Enter(NodeOrToken::Node(node)) => {
                    outline.rest.push_str(&format!("{:?}(", node.kind()));
                    if let Some(func) = reached_by_calls(&node) {
                        let range = func.syntax().text_range();
                        outline.bodies.push((range, words(&node)));
                        walk.skip_subtree();
                        outline.rest.push(')');
                    }
                }
                WalkEvent::Enter(NodeOrToken::Token(token)) => {
                    if !said_to_rustdoc_alone(&token) {
                        outline.rest.push_str(token.text());
                        outline.rest.push(' ');
                    }
                }
                WalkEvent::Leave(NodeOrToken::Node(_)) => outline.rest.push(')'),
                WalkEvent::Leave(NodeOrToken::Token(_)) => {}
            }
        }
        outline
    }
}

/// The function whose body `node` is, when that body is reached only by calling it: it is
/// written among items, outside any macro's input, and declares no macro, which can be
/// exported from it. A `const fn` is evaluated in types, so its body is more than calls see.
fn reached_by_calls(node: &SyntaxNode) -> Option<ast::Fn> {
    if node.kind() != SyntaxKind::BLOCK_EXPR {
        return None;
    }
    let func = node.parent().and_then(ast::Fn::cast)?;
    if func.const_token().is_some() {
        return None;
    }
    let declares_macro = node
        .descendants()
        .any(|it| matches!(it.kind(), SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF));
    let among_items = func.syntax().ancestors().skip(1).all(|it| {
        matches!(
            it.kind(),
            SyntaxKind::SOURCE_FILE
                | SyntaxKind::ITEM_LIST
                | SyntaxKind::ASSOC_ITEM_LIST
                | SyntaxKind::MODULE
                | SyntaxKind::IMPL
                | SyntaxKind::TRAIT
        )
    });
    (!declares_macro && among_items).then_some(func)
}

/// The tokens of a node but blank space and comments.
fn words(node: &SyntaxNode) -> String {
    let mut words = String::new();
    for token in node.descendants_with_tokens().filter_map(NodeOrToken::into_token) {
        if !token.kind().is_trivia()
            || token.parent().is_some_and(|it| it.kind() == SyntaxKind::TOKEN_TREE)
        {
            words.push_str(token.text());
            words.push(' ');
        }
    }
    words
}

/// Whether the token is blank space, a comment, or a doc comment only rustdoc reads: one on
/// an item that no attribute macro or derive is handed, or one around it. What is handed to a
/// macro is part of what it writes, blank space too.
fn said_to_rustdoc_alone(token: &syntax::SyntaxToken) -> bool {
    if token.parent().is_some_and(|it| it.kind() == SyntaxKind::TOKEN_TREE) {
        return false;
    }
    match token.kind() {
        SyntaxKind::WHITESPACE | SyntaxKind::COMMENT => true,
        SyntaxKind::OUTER_DOC_COMMENT | SyntaxKind::INNER_DOC_COMMENT => {
            token.parent_ancestors().all(|it| {
                ast::AnyHasAttrs::cast(it).is_none_or(|item| item.attrs().all(|it| is_inert(&it)))
            })
        }
        _ => false,
    }
}

/// Attributes that leave the item they are on as it is written, to the compiler.
fn is_inert(attr: &ast::Attr) -> bool {
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
        "test",
        "ignore",
        "should_panic",
    ];
    attr.simple_name().is_some_and(|name| INERT.contains(&name.as_str()))
}

/// What reaching one thing reaches.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Node {
    /// A function, static or constant: what its code calls and reads.
    Body(DefWithBodyId),
    /// A value of the type: the methods of its trait impls, which may run without a call
    /// that names them, and what dropping it runs.
    Adt(AdtId),
    /// What dropping a value of the type runs: its `Drop` and that of its fields.
    Drop(AdtId),
}

/// The impls of the program's crates, for following calls through traits and types.
struct Graph<'a> {
    db: &'a RootDatabase,
    seen: &'a FxHashSet<Crate>,
    /// The methods of each trait's impls, with the type each impl is for when that is a
    /// named type.
    by_trait: FxHashMap<hir_def::TraitId, Vec<(Option<AdtId>, FunctionId)>>,
    /// The methods of the trait impls whose header names each type.
    by_adt: FxHashMap<AdtId, Vec<FunctionId>>,
    /// The methods of the `Drop` impl of each type.
    drops: FxHashMap<AdtId, Vec<FunctionId>>,
}

impl<'a> Graph<'a> {
    fn new(db: &'a RootDatabase, seen: &'a FxHashSet<Crate>) -> Graph<'a> {
        let mut graph = Graph {
            db,
            seen,
            by_trait: FxHashMap::default(),
            by_adt: FxHashMap::default(),
            drops: FxHashMap::default(),
        };
        let mut headers: FxHashMap<ImplId, (Option<AdtId>, FxHashSet<AdtId>, bool)> =
            FxHashMap::default();
        for &krate in seen.iter().filter(|it| it.origin(db).is_local()) {
            for func in crate::functions_of(db, krate) {
                let ItemContainerId::ImplId(id) = func.lookup(db).container else { continue };
                let Some(trait_ref) = db.impl_trait(id) else { continue };
                let trait_id = trait_ref.skip_binder().def_id.0;
                let (on, adts, drop) = headers.entry(id).or_insert_with(|| {
                    let on = hir::Impl::from(id).self_ty(db).as_adt().map(AdtId::from);
                    let drop = hir::Trait::from(trait_id).name(db).as_str() == "Drop"
                        && hir::Trait::from(trait_id).module(db).krate(db).origin(db).is_lang();
                    (on, impl_header_adts(db, id), drop)
                });
                graph.by_trait.entry(trait_id).or_default().push((*on, func));
                for adt in adts.iter() {
                    graph.by_adt.entry(*adt).or_default().push(func);
                }
                if let (true, Some(on)) = (*drop, on) {
                    graph.drops.entry(*on).or_default().push(func);
                }
            }
        }
        graph
    }

    /// What `node` reaches in one step.
    fn next(&self, node: Node, out: &mut Vec<Node>) {
        let db = self.db;
        match node {
            Node::Body(def) => {
                let krate = Crate::from(def.module(db).krate(db));
                // What a library of crates.io calls of ours it is handed: a function, or a
                // value whose trait impls it calls, which the caller names.
                if !self.seen.contains(&krate) || !krate.origin(db).is_local() {
                    return;
                }
                let deps = body_deps(db, def);
                out.extend(deps.callees.iter().map(|&it| Node::Body(it.into())));
                out.extend(deps.adts.iter().map(|&it| Node::Adt(it)));
                out.extend(deps.bodies.iter().map(|&it| Node::Body(it)));
                for &(method, on) in &deps.trait_calls {
                    let ItemContainerId::TraitId(trait_id) = method.lookup(db).container else {
                        continue;
                    };
                    let name = &hir_def::signatures::FunctionSignature::of(db, method).name;
                    let impls = self.by_trait.get(&trait_id).into_iter().flatten();
                    for &(for_adt, func) in impls {
                        // A call on a named type runs its impl, or one for any type.
                        let runs = on.is_none() || for_adt.is_none() || for_adt == on;
                        if runs
                            && hir_def::signatures::FunctionSignature::of(db, func).name == *name
                        {
                            out.push(Node::Body(func.into()));
                        }
                    }
                }
            }
            Node::Adt(adt) => {
                let methods = self.by_adt.get(&adt).into_iter().flatten();
                out.extend(methods.map(|&it| Node::Body(it.into())));
                out.push(Node::Drop(adt));
            }
            Node::Drop(adt) => {
                let methods = self.drops.get(&adt).into_iter().flatten();
                out.extend(methods.map(|&it| Node::Body(it.into())));
                let local = |it: &AdtId| self.seen.contains(&Crate::from(it.module(db).krate(db)));
                out.extend(field_adts(db, adt).into_iter().filter(local).map(Node::Drop));
            }
        }
    }

    /// Of everything `roots` reach, what reaches one of `edited`, each with how the first
    /// edited body it reaches is shown.
    fn reaching(
        &self,
        roots: impl Iterator<Item = Node>,
        edited: &FxHashMap<DefWithBodyId, String>,
    ) -> FxHashMap<Node, String> {
        let mut callers: FxHashMap<Node, Vec<Node>> = FxHashMap::default();
        let mut stack: Vec<Node> = roots.collect();
        let mut visited: FxHashSet<Node> = stack.iter().copied().collect();
        let mut next = Vec::new();
        while let Some(node) = stack.pop() {
            next.clear();
            self.next(node, &mut next);
            for &it in &next {
                callers.entry(it).or_default().push(node);
                if visited.insert(it) {
                    stack.push(it);
                }
            }
        }
        let mut reaching: FxHashMap<Node, String> = FxHashMap::default();
        let mut queue: Vec<Node> = Vec::new();
        for (&body, shown) in edited {
            let node = Node::Body(body);
            if visited.contains(&node) {
                reaching.insert(node, shown.clone());
                queue.push(node);
            }
        }
        while let Some(node) = queue.pop() {
            let why = reaching[&node].clone();
            for &caller in callers.get(&node).into_iter().flatten() {
                if !reaching.contains_key(&caller) {
                    reaching.insert(caller, why.clone());
                    queue.push(caller);
                }
            }
        }
        reaching
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn bodies(was: &str, now: &str) -> Option<Vec<String>> {
        let ranges = edited_bodies(was, now, Edition::CURRENT)?;
        Some(ranges.into_iter().map(|it| now[it].lines().next().unwrap().to_owned()).collect())
    }

    #[test]
    fn an_edit_to_bodies_names_their_functions() {
        let was = "fn a() { 1; }\nfn b() { 2; }\n";
        assert_eq!(bodies(was, "fn a() { 1; }\nfn b() { 3; }\n").unwrap(), ["fn b() { 3; }"]);
        assert_eq!(bodies(was, "fn a() { 1; }\n\n// note\nfn b() { 2; }\n").unwrap(), [""; 0]);
        let method = "impl S {\n    fn m(&self) -> u8 { 1 }\n}\n";
        let edited = bodies(method, &method.replace("{ 1 }", "{ 2 }")).unwrap();
        assert_eq!(edited, ["fn m(&self) -> u8 { 2 }"]);
        let test = "#[test]\nfn t() { assert!(f()); }\n";
        assert_eq!(bodies(test, &test.replace("f()", "!f()")).unwrap().len(), 1);
        let attributed = "#[tokio::test]\nasync fn t() { f().await; }\n";
        assert_eq!(bodies(attributed, &attributed.replace("f()", "g()")).unwrap().len(), 1);
    }

    #[test]
    fn more_than_bodies_is_none() {
        for (was, now) in [
            ("fn a() {}", "fn b() {}"),
            ("fn a() -> u8 { 1 }", "fn a() -> u16 { 1 }"),
            ("fn a() {}", "fn a() {} fn b() {}"),
            ("struct S(u8);", "struct S(u16);"),
            ("const C: u8 = 1;", "const C: u8 = 2;"),
            ("static S: u8 = 1;", "static S: u8 = 2;"),
            ("const fn f() -> u8 { 1 }", "const fn f() -> u8 { 2 }"),
            ("macro_rules! m { () => { 1 } }", "macro_rules! m { () => { 2 } }"),
            (
                "fn f() { macro_rules! m { () => { 1 } } }",
                "fn f() { macro_rules! m { () => { 2 } } }",
            ),
            ("#[cfg(test)]\nfn f() {}", "#[cfg(not(test))]\nfn f() {}"),
            ("#[cfg(test)]\nmod t;", "mod t;"),
            ("m! { fn f() { 1; } }", "m! { fn f() { 2; } }"),
            ("type T = u8;", "type T = u16;"),
            ("use a::b;", "use a::c;"),
            ("const _: () = { fn f() { 1; } };", "const _: () = { fn f() { 2; } };"),
            ("static S: u8 = include!(\"a\");", "static S: u8 = include!(\"b\");"),
            ("/// A.\n#[derive(Parser)]\nstruct S;", "/// B.\n#[derive(Parser)]\nstruct S;"),
        ] {
            assert_eq!(bodies(was, now), None, "{was:?} to {now:?}");
        }
    }

    #[test]
    fn what_only_rustdoc_reads_is_no_change() {
        assert_eq!(bodies("/// A.\nfn f() {}", "/// B.\nfn f() {}").unwrap().len(), 0);
        assert_eq!(bodies("/// A.\nstruct S;", "/// B.\nstruct S;").unwrap().len(), 0);
        let inner = "fn f() {\n    // a\n    1;\n}\n";
        assert_eq!(bodies(inner, &inner.replace("// a", "// b")).unwrap().len(), 0);
    }

    #[test]
    fn a_body_in_a_body_is_its_outer_one() {
        let was = "fn f() { fn g() -> u8 { 1 } g(); }";
        let edited = bodies(was, &was.replace("{ 1 }", "{ 2 }")).unwrap();
        assert_eq!(edited, [was.replace("{ 1 }", "{ 2 }")]);
    }
}
