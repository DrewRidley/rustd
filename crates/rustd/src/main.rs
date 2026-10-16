//! Spike: keep a workspace loaded in rust-analyzer's database and compile
//! functions to native code that links against rustc-built artifacts.

mod cli;
mod incremental_link;
mod outline;
mod symbols;
mod tree;

/// The daemon holds millions of small allocations and frees a file's worth of them on each
/// edit, which the system allocator is slow at.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use ide_db::base_db::SourceDatabase as _;
use std::{
    io::{BufRead as _, BufReader, Write as _},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, bail};
use hir::{
    AsAssocItem, AssocItemContainer, Crate, HirDisplay as _, Module, ModuleDef, db::HirDatabase,
};
use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, ItemContainerId, Lookup, ModuleDefId, StaticId,
    expr_store::Body, item_scope::ItemScope,
};
use hir_ty::mir::codegen::{
    CodegenError, CodegenUnit, Rejections, Roots, SymbolResolver, exported_symbol,
};
use ide_db::{ChangeWithProcMacros, RootDatabase};
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace};
use project_model::{
    CargoConfig, CargoFeatures, ProjectManifest, ProjectWorkspace, ProjectWorkspaceKind,
    RustLibSource,
};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, SyntaxNode,
    ast::{self, HasName},
    match_ast,
};
use vfs::{AbsPathBuf, Vfs, VfsPath};

use crate::symbols::SymbolTable;

struct Args {
    project: PathBuf,
    krate: Option<String>,
    /// More crates to compile from the same loaded workspace, into the same object. When `out`
    /// is a directory, each crate is a program of its own instead: `out` and `report` get a
    /// file per crate, and a crate that fails gets a `.error` file instead of ending the run.
    batch: Vec<String>,
    out: PathBuf,
    symbol_files: Vec<PathBuf>,
    watch: bool,
    /// A Unix socket to take build requests on, instead of building when a file changes.
    socket: Option<PathBuf>,
    link_cmd: Option<String>,
    /// Where to write the object that amends `out` after an edit, for the link to take ahead of
    /// `out`. Without it every edit writes `out` again.
    patch: Option<PathBuf>,
    /// Tab-separated per-function outcome, for the harness.
    report: Option<PathBuf>,
    /// File of function paths to leave to rustc, for bisecting miscompiles.
    deny: Option<PathBuf>,
    /// Directory that gets one object per compiled function, for testing each in isolation.
    isolate: Option<PathBuf>,
    /// Build the workspace's crates as `cargo test` does, with `cfg(test)`.
    cfg_test: bool,
    /// Cargo features to build the workspace's crates with, on top of the default ones.
    features: Vec<String>,
    /// Target triple (e.g. `wasm32-unknown-unknown`).
    target: Option<String>,
}

impl Args {
    /// The directory of the project, which cargo runs rustc in.
    fn root(&self) -> anyhow::Result<PathBuf> {
        let mut root = std::fs::canonicalize(&self.project)?;
        if root.is_file() {
            root.pop();
        }
        Ok(root)
    }

    fn parse() -> anyhow::Result<Args> {
        let mut args = std::env::args().skip(1);
        let mut project = None;
        let mut krate = None;
        let mut batch = Vec::new();
        let mut out = None;
        let mut symbol_files = Vec::new();
        let mut watch = false;
        let mut socket = None;
        let mut link_cmd = None;
        let mut patch = None;
        let mut report = None;
        let mut deny = None;
        let mut isolate = None;
        let mut cfg_test = false;
        let mut features = Vec::new();
        let mut target = None;
        while let Some(arg) = args.next() {
            let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--crate" if krate.is_some() => batch.push(value()?),
                "--crate" => krate = Some(value()?),
                "--out" => out = Some(PathBuf::from(value()?)),
                "--symbols" => symbol_files.push(PathBuf::from(value()?)),
                "--link-cmd" => link_cmd = Some(value()?),
                "--patch" => patch = Some(PathBuf::from(value()?)),
                "--report" => report = Some(PathBuf::from(value()?)),
                "--deny" => deny = Some(PathBuf::from(value()?)),
                "--isolate" => isolate = Some(PathBuf::from(value()?)),
                "--watch" => watch = true,
                "--socket" => socket = Some(PathBuf::from(value()?)),
                "--cfg-test" => cfg_test = true,
                "--features" => features.extend(value()?.split(',').map(str::to_owned)),
                "--target" => target = Some(value()?),
                _ if project.is_none() => project = Some(PathBuf::from(arg)),
                _ => bail!("unexpected argument {arg}"),
            }
        }
        Ok(Args {
            project: project.context("missing project path")?,
            krate,
            batch,
            out: out.context("missing --out")?,
            symbol_files,
            watch,
            socket,
            link_cmd,
            patch,
            report,
            deny,
            isolate,
            cfg_test,
            features,
            target,
        })
    }
}

struct Resolver<'a> {
    db: &'a RootDatabase,
    table: &'a SymbolTable,
    /// What panic locations in the workspace are relative to.
    root: PathBuf,
    rustc_commit: std::cell::OnceCell<Option<String>>,
}

impl Resolver<'_> {
    /// The function's path as `rustc-demangle` prints its v0 symbol.
    fn path(&self, func: FunctionId) -> Option<String> {
        let ModuleDef::Function(func) = ModuleDef::from(ModuleDefId::FunctionId(func)) else {
            return None;
        };
        let name = func.name(self.db);
        let Some(assoc) = func.as_assoc_item(self.db) else {
            return self.item_path(func.module(self.db), name.as_str());
        };
        let imp = match assoc.container(self.db) {
            AssocItemContainer::Impl(imp) => imp,
            AssocItemContainer::Trait(trait_) => {
                let trait_path =
                    self.item_path(trait_.module(self.db), trait_.name(self.db).as_str())?;
                return Some(format!("{trait_path}::{}", name.as_str()));
            }
        };
        let self_ty = self.type_path(&imp.self_ty(self.db))?;
        match imp.trait_(self.db) {
            Some(trait_) => {
                let mut path =
                    self.item_path(trait_.module(self.db), trait_.name(self.db).as_str())?;
                // `impl PartialEq<str> for String` and `impl PartialEq<&str> for String` are
                // told apart by the trait's arguments alone.
                let trait_ref = imp.trait_ref(self.db)?;
                let params = hir::GenericDef::from(trait_).type_or_const_params(self.db);
                let mut args = Vec::new();
                for (idx, param) in params.iter().enumerate().skip(1) {
                    let arg = trait_ref.get_type_argument(idx)?;
                    let defaulted = param
                        .as_type_param(self.db)
                        .is_some_and(|it| it.default(self.db).is_some());
                    // FIXME: the only default understood is `= Self`.
                    let is_self = hir::attach_db(self.db, || arg == trait_ref.self_ty());
                    if defaulted && idx + 1 == params.len() && is_self {
                        break;
                    }
                    args.push(self.type_path(&arg)?);
                }
                if !args.is_empty() {
                    path = format!("{path}<{}>", args.join(", "));
                }
                let trait_ = path;
                Some(format!("<{self_ty} as {trait_}>::{}", name.as_str()))
            }
            None => Some(format!("<{self_ty}>::{}", name.as_str())),
        }
    }

    /// The file the crate's source starts at.
    fn crate_root(&self, krate: Crate) -> Option<PathBuf> {
        let file = krate.root_file(self.db);
        let source_root = self.db.file_source_root(file).source_root_id(self.db);
        let source_root = self.db.source_root(source_root).source_root(self.db);
        Some(PathBuf::from(source_root.path_for_file(&file)?.as_path()?.to_string()))
    }

    fn item_path(&self, module: Module, name: &str) -> Option<String> {
        let mut path = self.scope_path(module)?;
        path.push_str("::");
        path.push_str(name);
        Some(path)
    }

    /// The function written at `func`, which holds the block that `scope` is the module of.
    ///
    /// `Semantics::to_def` answers the same, but starts from every crate the file is part of,
    /// which for a package with tests and examples analyses all its dev-dependencies.
    fn function_at(
        &self,
        sema: &hir::Semantics<'_, RootDatabase>,
        scope: Module,
        func: &ast::Fn,
    ) -> Option<hir::Function> {
        use hir::HasSource;
        let db = self.db;
        let file = sema.hir_file_for(func.syntax());
        let range = func.syntax().text_range();
        let is_it = |it: &hir::Function| {
            it.source(db).is_some_and(|source| {
                source.file_id == file && source.value.syntax().text_range() == range
            })
        };
        let among = |items: Vec<hir::AssocItem>| {
            items.into_iter().filter_map(hir::AssocItem::as_function).find(is_it)
        };
        let mut module = Some(scope);
        while let Some(it) = module {
            let found = it
                .declarations(db)
                .into_iter()
                .find_map(|def| match def {
                    ModuleDef::Function(func) => Some(func).filter(is_it),
                    ModuleDef::Trait(trait_) => among(trait_.items(db)),
                    _ => None,
                })
                .or_else(|| it.impl_defs(db).into_iter().find_map(|imp| among(imp.items(db))));
            if found.is_some() || it.nearest_non_block_module(db) == it {
                return found;
            }
            module = it.parent(db);
        }
        None
    }

    /// The path of what holds a module's items: the module, or for the items of a block the
    /// function, closure, constant or static the block is in, which only the syntax knows.
    fn scope_path(&self, module: Module) -> Option<String> {
        if module.nearest_non_block_module(self.db) == module {
            let krate = module.krate(self.db);
            let krate_name = krate.display_name(self.db)?;
            let mut path = self
                .table
                .crate_path(krate_name.crate_name().as_str(), &self.crate_root(krate)?)?;
            for segment in module.path_segments(self.db) {
                path.push_str("::");
                path.push_str(segment.as_str());
            }
            return Some(path);
        }
        let sema = hir::Semantics::new(self.db);
        let source = module.definition_source(self.db);
        let hir::ModuleSource::BlockExpr(block) = source.value else { return None };
        let range = block.syntax().text_range();
        let block = sema
            .parse_or_expand(source.file_id)
            .covering_element(range)
            .ancestors()
            .find(|it| it.text_range() == range && ast::BlockExpr::can_cast(it.kind()))?;
        let mut inner = Vec::new();
        let mut owner = None;
        for node in sema.ancestors_with_macros(block) {
            if is_closure(&node) {
                inner.push(format!("{{closure#{}}}", closure_number(&sema, &node)?));
                continue;
            }
            if let Some(func) = ast::Fn::cast(node.clone()) {
                // The body of an `async fn` is a closure to rustc.
                if func.async_token().is_some() {
                    inner.push("{closure#0}".to_owned());
                }
                let func = self.function_at(&sema, module, &func)?;
                owner = Some(self.path(func.try_into().ok()?)?);
                break;
            }
            match_ast! {
                match node {
                    ast::Const(it) => inner.push(it.name().map_or("_".to_owned(), |it| it.text().to_string())),
                    ast::Static(it) => inner.push(it.name()?.text().to_string()),
                    // Anonymous constants have numbers we don't know.
                    ast::BlockExpr(it) => if it.const_token().is_some() { return None },
                    ast::ConstArg(_) => return None,
                    _ => {},
                }
            }
        }
        let mut path = match owner {
            Some(owner) => owner,
            None => {
                let outer = inner.pop()?;
                self.item_path(module.nearest_non_block_module(self.db), &outer)?
            }
        };
        for segment in inner.iter().rev() {
            path.push_str("::");
            path.push_str(segment);
        }
        Some(path)
    }

    /// A type the way rustc writes it in a symbol.
    fn type_path(&self, ty: &hir::Type<'_>) -> Option<String> {
        if let Some((inner, mutability)) = ty.as_reference() {
            let inner = self.type_path(&inner)?;
            return Some(match mutability {
                hir::Mutability::Shared => format!("&{inner}"),
                hir::Mutability::Mut => format!("&mut {inner}"),
            });
        }
        if let Some(elem) = ty.as_slice() {
            return Some(format!("[{}]", self.type_path(&elem)?));
        }
        if let Some(builtin) = ty.as_builtin() {
            return Some(builtin.name().as_str().to_owned());
        }
        // A parameter of the `impl` an item is in, where the item is not generic itself: a
        // static, or what a closure or constant in a generic function holds.
        if ty.as_type_param(self.db).is_some() {
            return Some("_".to_owned());
        }
        let adt = ty.as_adt()?;
        let path = self.item_path(adt.module(self.db), adt.name(self.db).as_str())?;
        let generics = hir::GenericDef::from(adt);
        let params = generics.type_or_const_params(self.db);
        if params.is_empty() {
            return Some(path);
        }
        // rustc leaves a defaulted const argument out of the name, so `Bump` may be `Bump<1>`.
        // FIXME: name those too.
        if !params.iter().all(|it| it.as_type_param(self.db).is_some()) {
            return None;
        }
        // Lifetimes are erased, and rustc leaves erased ones out of the name.
        let args: Option<Vec<_>> = ty.type_arguments().map(|arg| self.type_path(&arg)).collect();
        Some(format!("{path}<{}>", args?.join(", ")))
    }
}

/// The commit of the toolchain installed at `toolchain`, which rustc puts in the names of the
/// standard library's files.
fn rustc_commit(toolchain: &str) -> Option<String> {
    let manifest =
        std::fs::read_to_string(format!("{toolchain}lib/rustlib/multirust-channel-manifest.toml"))
            .ok()?;
    let (_, rust) = manifest.split_once("[pkg.rust]")?;
    let (_, commit) = rust.split_once("git_commit_hash = \"")?;
    Some(commit.split_once('"')?.0.to_owned())
}

impl SymbolResolver for Resolver<'_> {
    fn in_parallel(&self, count: usize, work: &(dyn Fn(&dyn HirDatabase, usize) + Sync)) {
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get()).min(count);
        if threads < 2 {
            return;
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let db = self.db.clone();
                let next = &next;
                scope.spawn(move || {
                    hir::attach_db(&db, || {
                        loop {
                            let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if at >= count {
                                break;
                            }
                            work(&db, at);
                        }
                    })
                });
            }
        });
    }

    fn source_path(&self, file: vfs::FileId) -> Option<String> {
        let source_root = self.db.file_source_root(file).source_root_id(self.db);
        let source_root = self.db.source_root(source_root).source_root(self.db);
        let path = source_root.path_for_file(&file)?.as_path()?.to_string();
        if let Some((toolchain, file)) = path.split_once("lib/rustlib/src/rust/")
            && let Some(commit) = self.rustc_commit.get_or_init(|| rustc_commit(toolchain))
        {
            return Some(format!("/rustc/{commit}/{file}"));
        }
        let relative = Path::new(&path).strip_prefix(&self.root).ok();
        Some(relative.map_or(path.clone(), |it| it.to_string_lossy().into_owned()))
    }

    fn function(&self, func: FunctionId) -> Option<String> {
        if let Some(symbol) = exported_symbol(self.db, func) {
            return Some(symbol);
        }
        let path = self.path(func)?;
        if let Some(sym) = self.table.get(&path) {
            return Some(sym.to_owned());
        }
        if hir_def::attrs::AttrFlags::query(self.db, func.into())
            .contains(hir_def::attrs::AttrFlags::IS_TEST)
        {
            let rel = path.split_once("::").map_or(path.as_str(), |(_, rest)| rest);
            return Some(format!("__rustd_new_test__{}", rel.replace("::", "__")));
        }
        None
    }

    fn stable_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        let krate = Crate::from(krate);
        let name = krate.display_name(self.db)?;
        let root = self.crate_root(krate);
        root.as_deref()
            .and_then(|r| self.table.stable_crate_id(name.crate_name().as_str(), r))
            .or_else(|| {
                use std::hash::{Hash, Hasher};
                let mut h = rustc_hash::FxHasher::default();
                name.crate_name().as_str().hash(&mut h);
                Some(h.finish())
            })
    }

    fn declared_in(&self, adt: hir_def::AdtId) -> Option<FunctionId> {
        let adt = hir::Adt::from(adt);
        let sema = hir::Semantics::new(self.db);
        let source = sema.source(adt)?;
        let mut around = sema.ancestors_with_macros(source.value.syntax().clone()).skip(1);
        let func = ast::Fn::cast(around.find(is_definition)?)?;
        // The body of an `async fn` is a closure to rustc.
        if func.async_token().is_some() {
            return None;
        }
        let name = adt.name(self.db);
        let named_so = func.syntax().descendants().filter_map(ast::Name::cast).filter(|it| {
            it.text() == name.as_str()
                && it.syntax().parent().is_some_and(|it| {
                    ast::Adt::can_cast(it.kind())
                        || ast::Trait::can_cast(it.kind())
                        || ast::TypeAlias::can_cast(it.kind())
                        || ast::Module::can_cast(it.kind())
                })
        });
        if named_so.count() != 1 {
            return None;
        }
        self.function_at(&sema, adt.module(self.db), &func)?.try_into().ok()
    }

    fn impl_place(&self, id: hir_def::ImplId) -> Option<u32> {
        use hir::HasSource;
        let db = self.db;
        let it = hir::Impl::from(id);
        let source = it.source(db)?;
        // FIXME: where rustc puts the `impl`s a macro makes among the ones written out.
        if source.file_id.is_macro() {
            return None;
        }
        let start = |it: &hir::InFile<ast::Impl>| it.value.syntax().text_range().start();
        let mut earlier = 0;
        for other in it.module(db).impl_defs(db) {
            let other = other.source(db)?;
            if other.file_id != source.file_id {
                return None;
            }
            earlier += u32::from(start(&other) < start(&source));
        }
        Some(earlier)
    }

    fn static_(&self, id: StaticId) -> Option<String> {
        let ModuleDef::Static(it) = ModuleDef::from(ModuleDefId::StaticId(id)) else {
            return None;
        };
        let module = it.module(self.db);
        let name = it.name(self.db);
        if module.nearest_non_block_module(self.db) == module {
            let path = self.item_path(module, name.as_str())?;
            return self.table.get(&path).map(str::to_owned);
        }
        // A static in a body is named after the items around it, which only the syntax knows.
        let sema = hir::Semantics::new(self.db);
        let source = sema.source(it)?;
        let mut names = vec![name.as_str().to_owned()];
        let mut closures = Vec::new();
        let mut owner = None;
        for node in sema.ancestors_with_macros(source.value.syntax().clone()).skip(1) {
            if is_closure(&node) {
                closures.push(closure_number(&sema, &node)?);
                continue;
            }
            if let Some(func) = ast::Fn::cast(node.clone()) {
                if func.async_token().is_some() {
                    closures.push(0);
                }
                let func = self.function_at(&sema, module, &func)?;
                owner = Some(self.path(func.try_into().ok()?)?);
                break;
            }
            let name = match_ast! {
                match node {
                    ast::Const(it) => it.name(),
                    ast::Static(it) => it.name(),
                    _ => None,
                }
            };
            names.extend(name.map(|it| it.text().to_string()));
        }
        let owner = match owner {
            Some(owner) => owner,
            None => {
                let outer = names.pop()?;
                self.item_path(module.nearest_non_block_module(self.db), &outer)?
            }
        };
        names.push(owner);
        names.reverse();
        closures.reverse();
        let path = names.join("::");
        // Only a path through a closure or an anonymous constant has segments left out of it.
        let named = |node: &SyntaxNode| {
            ast::Static::cast(node.clone())
                .is_some_and(|it| it.name().is_some_and(|it| it.text() == name.as_str()))
        };
        let place = place_in_definition(&sema, source.value.syntax(), &named)?.into();
        let symbol = self
            .table
            .get_nested(&path, &closures, place)
            .or_else(|| self.table.get_at(&path, place));
        symbol.map(str::to_owned)
    }

    fn path(&self, path: &str) -> Option<String> {
        self.table.get(path).map(str::to_owned)
    }

    fn item(&self, item: ModuleDefId) -> Option<String> {
        let item = ModuleDef::from(item);
        self.item_path(item.module(self.db)?, item.name(self.db)?.as_str())
    }
}

/// Whether rustc numbers the node as a closure: a closure, or an `async` block.
fn is_closure(node: &SyntaxNode) -> bool {
    ast::ClosureExpr::can_cast(node.kind())
        || ast::BlockExpr::cast(node.clone()).is_some_and(|it| it.async_token().is_some())
}

fn is_definition(node: &SyntaxNode) -> bool {
    !ast::MacroCall::can_cast(node.kind())
        && (ast::Item::can_cast(node.kind())
            || ast::AssocItem::can_cast(node.kind())
            || ast::ConstArg::can_cast(node.kind())
            || is_closure(node)
            || ast::BlockExpr::cast(node.clone()).is_some_and(|it| it.const_token().is_some()))
}

/// The place of a node among those of the definition it is written in that `counts` holds
/// of, in the order rustc comes across them. This is how rustc tells apart the closures
/// of a function, and the statics of one name that a macro used twice in it declares.
fn place_in_definition(
    sema: &hir::Semantics<'_, RootDatabase>,
    target: &SyntaxNode,
    counts: &dyn Fn(&SyntaxNode) -> bool,
) -> Option<u32> {
    let mut around = sema.ancestors_with_macros(target.clone()).skip(1);
    let mut root = None;
    let owner = around.find(|node| {
        root = Some(node.clone());
        is_definition(node)
    });
    let mut earlier = 0;
    Place { sema, target, counts, earlier: &mut earlier }
        .find_in(&owner.or(root)?)
        .then_some(earlier)
}

struct Place<'a, 'db> {
    sema: &'a hir::Semantics<'db, RootDatabase>,
    target: &'a SyntaxNode,
    counts: &'a dyn Fn(&SyntaxNode) -> bool,
    earlier: &'a mut u32,
}

impl Place<'_, '_> {
    /// Counts the nodes before the target in the definition `node` is of. Whether the target
    /// is there.
    ///
    /// rustc numbers what is written in the definition before what the macros used in it
    /// expand to, and then goes through the macros in turn, each the same way.
    fn find_in(&mut self, node: &SyntaxNode) -> bool {
        let mut calls = Vec::new();
        if self.find_written_in(node, &mut calls) {
            return true;
        }
        calls.iter().any(|call| {
            self.sema
                .expand_macro_call(call)
                .is_some_and(|expansion| self.find_in(&expansion.value))
        })
    }

    /// As `find_in`, of what is not in a macro: `calls` gets the macros come across.
    fn find_written_in(&mut self, node: &SyntaxNode, calls: &mut Vec<ast::MacroCall>) -> bool {
        for child in node.children() {
            if let Some(call) = ast::MacroCall::cast(child.clone()) {
                calls.push(call);
                continue;
            }
            if child == *self.target {
                return true;
            }
            if (self.counts)(&child) {
                *self.earlier += 1;
            }
            if !is_definition(&child) && self.find_written_in(&child, calls) {
                return true;
            }
        }
        false
    }
}

/// The `n` of the `{closure#n}` rustc names a closure: its place among the closures of the
/// definition it is written in, a closure's own closures being counted apart.
fn closure_number(sema: &hir::Semantics<'_, RootDatabase>, closure: &SyntaxNode) -> Option<u32> {
    place_in_definition(sema, closure, &is_closure)
}

/// The crates called `krate`: a package's library and its binary may share the name, and both
/// are being built.
fn crates_named(db: &RootDatabase, krate: &str) -> anyhow::Result<Vec<Crate>> {
    let mut found: Vec<_> = Crate::all(db)
        .into_iter()
        .filter(|it| it.display_name(db).is_some_and(|name| name.crate_name().as_str() == krate))
        .collect();
    // A dependency may be another version of the crate being built.
    if found.iter().any(|it| it.origin(db).is_local()) {
        found.retain(|it| it.origin(db).is_local());
    }
    anyhow::ensure!(!found.is_empty(), "crate {krate} not found in workspace");
    Ok(found)
}

/// Resolves the names and collects the impls of the crates being built and of everything they
/// depend on, a crate per thread. Every build needs them, and one thread would do them in turn.
/// `load_workspace_at`, with each package's features those of a build of the package at
/// `project` alone, as the artifacts we link against were built.
fn load(
    project: &Path,
    cargo_config: &CargoConfig,
    load_config: &LoadCargoConfig,
) -> anyhow::Result<(RootDatabase, vfs::Vfs, impl Sized + use<>)> {
    let root = AbsPathBuf::assert_utf8(std::env::current_dir()?.join(project));
    let manifest = ProjectManifest::discover_single(&root)?;
    let mut workspace = ProjectWorkspace::load(manifest.clone(), cargo_config, &|_| {})?;
    if let ProjectWorkspaceKind::Cargo { cargo, .. } = &mut workspace.kind {
        if !cargo_config.set_test {
            cargo.drop_dev_dependencies();
        }
        let built = built_features(manifest.manifest_path().as_ref(), cargo_config)?;
        let packages: Vec<_> = cargo.packages().collect();
        for package in packages {
            let key = (cargo[package].name.clone(), cargo[package].version.to_string());
            if let Some(features) = built.get(&key) {
                cargo.set_active_features(package, features.iter().cloned().collect());
            }
        }
    }
    if load_config.load_out_dirs_from_check {
        let build_scripts = workspace.run_build_scripts(cargo_config, &|_| {})?;
        workspace.set_build_scripts(build_scripts);
    }
    load_workspace(workspace, &cargo_config.extra_env, load_config)
}

/// The features of each package, by name and version, in a build of the package of `manifest`.
///
/// `cargo metadata` unifies features over every member of the workspace, and cargo doesn't
/// when it builds one of them.
// FIXME: a package built for the host with other features than for the target has both here.
fn built_features(
    manifest: &Path,
    cargo_config: &CargoConfig,
) -> anyhow::Result<FxHashMap<(String, String), std::collections::BTreeSet<String>>> {
    let stdout_text = if let Ok(cache_path) = std::env::var("RUSTD_CACHE_TREE")
        && let Ok(expected_fp) = std::env::var("RUSTD_CACHE_FP")
        && let Ok(text) = std::fs::read_to_string(&cache_path)
        && let Some((fp_line, body)) = text.split_once('\n')
        && fp_line == expected_fp
    {
        body.to_owned()
    } else {
        let mut command = Command::new("cargo");
        let edges = if cargo_config.set_test { "normal,dev" } else { "normal" };
        command.args(["tree", "--edges", edges, "--prefix", "none", "--format", "{p}|{f}"]);
        command.arg("--manifest-path").arg(manifest);
        if let Ok(pkg) = std::env::var("RUSTD_PKG")
            && !pkg.is_empty()
        {
            command.args(["-p", &pkg]);
        }
        if let CargoFeatures::Selected { features, .. } = &cargo_config.features
            && !features.is_empty()
        {
            command.arg("--features").arg(features.join(","));
        }
        let output = command.output().context("running `cargo tree`")?;
        anyhow::ensure!(
            output.status.success(),
            "`cargo tree` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout)?;
        if let Ok(cache_path) = std::env::var("RUSTD_CACHE_TREE")
            && let Ok(fp) = std::env::var("RUSTD_CACHE_FP")
        {
            let _ = std::fs::write(&cache_path, format!("{fp}\n{text}"));
        }
        text
    };
    let mut built = FxHashMap::default();
    for line in stdout_text.lines() {
        // `name v1.2.3 (/path/of/a/member)|feature,feature (*)`
        let Some((package, features)) = line.split_once('|') else { continue };
        let mut package = package.split(' ');
        let (Some(name), Some(version)) = (package.next(), package.next()) else { continue };
        let version = version.trim_start_matches('v');
        let features = features.trim_end_matches(" (*)").split(',').filter(|it| !it.is_empty());
        built
            .entry((name.to_owned(), version.to_owned()))
            .or_insert_with(std::collections::BTreeSet::new)
            .extend(features.map(str::to_owned));
    }
    Ok(built)
}

fn resolve_crates(db: &RootDatabase, args: &Args) -> anyhow::Result<usize> {
    let mut seen = FxHashSet::default();
    let mut crates = Vec::new();
    let roots: Vec<Crate> = if let Some(first) = &args.krate {
        let mut r = Vec::new();
        for name in std::iter::once(first).chain(&args.batch) {
            r.extend(crates_named(db, name)?);
        }
        r
    } else if let Ok(bin_crate) = std::env::var("RUSTD_BIN_CRATE")
        && let Ok(r) = crates_named(db, &bin_crate.replace('-', "_"))
    {
        r
    } else {
        Crate::all(db).into_iter().filter(|it| it.origin(db).is_local()).collect()
    };
    for krate in roots {
        let krate: ide_db::base_db::Crate = krate.into();
        crates.extend(krate.transitive_deps(db).into_iter().filter(|&dep| seen.insert(dep)));
    }
    // A crate with fewer transitive dependencies must be resolved before one that depends on it.
    crates.sort_by_cached_key(|&c| c.transitive_deps(db).len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let db = db.clone();
            let (crates, next) = (&crates, &next);
            scope.spawn(move || {
                while let Some(&krate) =
                    crates.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                {
                    let start = Instant::now();
                    hir::attach_db(&db, || {
                        _ = hir::crate_def_map(&db, krate);
                        let names = start.elapsed();
                        _ = hir::TraitImpls::for_crate(&db, krate);
                        if std::env::var_os("RUSTD_STATS").is_some() {
                            let name = krate.extra_data(&db).display_name.as_ref();
                            let name = name.map(|it| it.to_string()).unwrap_or_default();
                            eprintln!(
                                "resolved {name}: names {names:.1?}, impls {:.1?}",
                                start.elapsed() - names
                            );
                        }
                    });
                }
            });
        }
    });
    Ok(crates.len())
}

/// The functions of the crates being built, listed once and again for the crates an edit can
/// have changed the items of.
#[derive(Default)]
struct Listed {
    functions: FxHashMap<Crate, ListedCrate>,
}

struct ListedCrate {
    functions: Vec<FunctionId>,
    /// For each, whether it is `main` or something a runtime looks up by name.
    starts: Vec<bool>,
    has_main: bool,
}

impl ListedCrate {
    fn new(db: &RootDatabase, krate: Crate) -> ListedCrate {
        let functions = functions_of(db, krate);
        let mut has_main = false;
        let starts = functions
            .iter()
            .map(|&func| {
                let is_main = hir::Function::from(func).is_main(db);
                has_main |= is_main;
                is_main || exported_symbol(db, func).is_some()
            })
            .collect();
        ListedCrate { functions, starts, has_main }
    }
}

impl Listed {
    fn of(&mut self, db: &RootDatabase, krate: Crate) -> &ListedCrate {
        self.functions.entry(krate).or_insert_with(|| ListedCrate::new(db, krate))
    }

    /// Notes that a file changed: one of `crates`, or with `None` one we can't place.
    fn touch(&mut self, db: &RootDatabase, crates: Option<&[ide_db::base_db::Crate]>) {
        let Some(crates) = crates else {
            self.functions.clear();
            return;
        };
        for &krate in crates {
            for seeing in krate.transitive_rev_deps(db) {
                self.functions.remove(&Crate::from(seeing));
            }
        }
    }
}

fn functions_of(db: &RootDatabase, krate: Crate) -> Vec<FunctionId> {
    let start = Instant::now();
    // The first thing asked of a crate after an edit: name resolution is redone here.
    hir_def::nameres::crate_def_map(db, krate.into());
    let resolved = start.elapsed();
    let mut modules: Vec<Module> = vec![krate.root_module(db)];
    let mut functions = Vec::new();
    // Each scope's functions, and the bodies whose blocks declare more scopes.
    let mut bodies: Vec<DefWithBodyId> = Vec::new();
    let mut scope = |scope: &ItemScope, bodies: &mut Vec<DefWithBodyId>| {
        for def in scope.declarations() {
            match def {
                // A function declared in an `extern` block has no body to compile.
                ModuleDefId::FunctionId(id)
                    if !matches!(id.lookup(db).container, ItemContainerId::ExternBlockId(_)) =>
                {
                    functions.push(id);
                    bodies.push(id.into());
                }
                ModuleDefId::ConstId(id) => bodies.push(id.into()),
                ModuleDefId::StaticId(id) => bodies.push(id.into()),
                _ => {}
            }
        }
        bodies.extend(scope.unnamed_consts().map(DefWithBodyId::from));
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                match item {
                    AssocItemId::FunctionId(id) => {
                        functions.push(id);
                        bodies.push(id.into());
                    }
                    AssocItemId::ConstId(id) => bodies.push(id.into()),
                    AssocItemId::TypeAliasId(_) => {}
                }
            }
        }
    };
    while let Some(module) = modules.pop() {
        modules.extend(module.children(db));
        let id = hir_def::ModuleId::from(module);
        scope(id.def_map(db).module(db, id).scope(db), &mut bodies);
    }
    let scopes = start.elapsed();
    let count = bodies.len();
    // A body's blocks include those of the blocks in it, but not those of the items in them.
    while !bodies.is_empty() {
        let round = lower(db, std::mem::take(&mut bodies));
        for body in round {
            for (_, def_map) in Body::of(db, body).blocks(db) {
                for (_, module) in def_map.modules(db) {
                    scope(module.scope(db), &mut bodies);
                }
            }
        }
    }
    if std::env::var_os("RUSTD_STATS").is_some() {
        eprintln!(
            "listing: name resolution {resolved:.1?}, scopes {:.1?}, {count} bodies {:.1?}",
            scopes - resolved,
            start.elapsed() - scopes
        );
    }
    functions
}

/// Lowers the bodies on all threads and hands them back. An edit to a file has every body in
/// it lowered again, which is most of the work of looking through them.
fn lower(db: &RootDatabase, bodies: Vec<DefWithBodyId>) -> Vec<DefWithBodyId> {
    let probing = PROBING.get();
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    std::thread::scope(|scope| {
        // The bodies of one file are next to each other, and the edited file's are the slow
        // ones: each thread takes every nth, so that they are spread over all of them.
        for part in 0..threads.min(bodies.len()) {
            let (db, bodies) = (db.clone(), &bodies);
            scope.spawn(move || {
                PROBING.set(probing);
                hir::attach_db(&db, || {
                    for &body in bodies.iter().skip(part).step_by(threads) {
                        Body::of(&db, body);
                    }
                })
            });
        }
    });
    bodies
}

struct Build {
    compiled: usize,
    skipped: Vec<(String, CodegenError)>,
    instances: usize,
    codegen: Duration,
    emit: Duration,
    /// How long finding the roots, checking what is cached and compiling took, in that order.
    steps: [Duration; 3],
    /// Within the checking, the longest a thread took over each kind of cached thing.
    looked: hir_ty::mir::codegen::StaleSteps,
}

thread_local! {
    /// Set on a thread that asks about what an edit may have removed. A query about something
    /// that is gone panics, which is the answer and not worth a report.
    pub(crate) static PROBING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn build(
    db: &RootDatabase,
    table: &SymbolTable,
    args: &Args,
    known: &mut Rejections,
    listed: &mut Listed,
    dirty_files: &FxHashSet<vfs::FileId>,
    initial_serials: &FxHashMap<FunctionId, u64>,
    changed_adts: &FxHashSet<hir_def::AdtId>,
    changed_const_names: &FxHashSet<String>,
    is_cold: bool,
) -> anyhow::Result<Build> {
    let start = Instant::now();
    let root = args.root()?;
    let resolver = Resolver { db, table, root, rustc_commit: Default::default() };
    let hir_db: &dyn HirDatabase = db;
    let mut local = Vec::new();
    let (entries, on_demand): (Vec<FunctionId>, Vec<FunctionId>) = if let Some(first) = &args.krate {
        let mut functions = Vec::new();
        let mut has_main = false;
        for krate in std::iter::once(first).chain(&args.batch) {
            for krate in crates_named(db, krate)? {
                local.push(krate);
                let listed = listed.of(db, krate);
                has_main |= listed.has_main;
                functions
                    .extend(listed.functions.iter().copied().zip(listed.starts.iter().copied()));
            }
        }
        let (e, o): (Vec<_>, Vec<_>) =
            functions.into_iter().partition(|&(_, starts)| starts || !has_main);
        let unzip = |it: Vec<(FunctionId, bool)>| it.into_iter().map(|it| it.0).collect::<Vec<_>>();
        (unzip(e), unzip(o))
    } else {
        let mut e = Vec::new();
        let mut o = Vec::new();
        let mut seen_crates = FxHashSet::default();
        let mut candidate_crates = Vec::new();
        for &file in dirty_files {
            for &krate in ide_db::base_db::relevant_crates(db, file) {
                if seen_crates.insert(krate) {
                    candidate_crates.push(Crate::from(krate));
                }
            }
        }
        if !dirty_files.is_empty()
            && let Ok(bc) = std::env::var("RUSTD_BIN_CRATE")
            && let Ok(bin_crates) = crates_named(db, &bc)
        {
            for bc_krate in bin_crates {
                if seen_crates.insert(bc_krate.into()) {
                    let l = listed.of(db, bc_krate);
                    if l.functions.len() <= 500 {
                        candidate_crates.push(bc_krate);
                    }
                }
            }
        }
        let mut global_need_callers = FxHashSet::default();
        for c in candidate_crates {
            local.push(c);
            let l = listed.of(db, c);
            let warm_deps = |db: &RootDatabase, funcs: &[FunctionId]| {
                let threads =
                    std::thread::available_parallelism().map_or(1, |it| it.get());
                std::thread::scope(|scope| {
                    for part in 0..threads.min(funcs.len()) {
                        let db = db.clone();
                        scope.spawn(move || {
                            hir::attach_db(&db, || {
                                for &func in funcs.iter().skip(part).step_by(threads) {
                                    let _ =
                                        hir_ty::mir::codegen::function_deps(&db, func);
                                }
                            });
                        });
                    }
                });
            };
            if !changed_adts.is_empty() || !global_need_callers.is_empty() {
                warm_deps(db, &l.functions);
            }
            let mut direct_changed = FxHashSet::default();
            let mut need_callers = global_need_callers.clone();
            let mut vtable_adts = changed_adts.clone();
            hir::attach_db(db, || {
                for &func in &l.functions {
                    let f = func
                        .lookup(db)
                        .id
                        .file_id
                        .original_file_respecting_includes(db)
                        .file_id(db);
                    let in_dirty_changed = dirty_files.contains(&f)
                        && initial_serials.get(&func).is_none_or(|&orig_s| {
                            hir_ty::mir::codegen::function_tree_serial(db, func) != orig_s
                        });
                    let touches_changed_const = !changed_const_names.is_empty()
                        && {
                            let idents =
                                hir_ty::mir::codegen::function_path_idents(db, func);
                            changed_const_names.iter().any(|n| idents.contains(n))
                        };
                    let touches_changed_adt =
                        hir_ty::mir::codegen::function_mentions_adts(db, func, &vtable_adts);
                    if in_dirty_changed || touches_changed_const || touches_changed_adt {
                        direct_changed.insert(func);
                        if initial_serials.contains_key(&func)
                            && (resolver.function(func).is_none()
                                || hir_def::signatures::FunctionSignature::of(db, func).is_async()
                                || hir_def::attrs::AttrFlags::query(db, func.into())
                                    .contains(hir_def::attrs::AttrFlags::RUSTC_FORCE_INLINE))
                        {
                            need_callers.insert(func);
                            let h_fn = hir::Function::from(func);
                            if let Some(assoc) = h_fn.as_assoc_item(db)
                                && let AssocItemContainer::Impl(imp) = assoc.container(db)
                                && let Some(adt) = imp.self_ty(db).as_adt()
                            {
                                vtable_adts.insert(hir_def::AdtId::from(adt));
                            }
                        }
                    }
                }
            });
            if !need_callers.is_empty() || vtable_adts.len() > changed_adts.len() {
                if changed_adts.is_empty() && global_need_callers.is_empty() {
                    warm_deps(db, &l.functions);
                }
                hir::attach_db(db, || {
                    for _ in 0..3 {
                        let mut newly_added = Vec::new();
                        for &func in &l.functions {
                            if !direct_changed.contains(&func)
                                && (hir_ty::mir::codegen::function_calls_any(
                                    db,
                                    func,
                                    &need_callers,
                                ) || hir_ty::mir::codegen::function_mentions_adts(
                                    db,
                                    func,
                                    &vtable_adts,
                                ))
                            {
                                newly_added.push(func);
                            }
                        }
                        if newly_added.is_empty() {
                            break;
                        }
                        for f in newly_added {
                            direct_changed.insert(f);
                            if resolver.function(f).is_none()
                                || hir_def::signatures::FunctionSignature::of(db, f).is_async()
                                || hir_def::attrs::AttrFlags::query(db, f.into())
                                    .contains(hir_def::attrs::AttrFlags::RUSTC_FORCE_INLINE)
                            {
                                need_callers.insert(f);
                            }
                        }
                    }
                });
            }
            global_need_callers.extend(need_callers);
            for &func in &l.functions {
                if direct_changed.contains(&func) && resolver.function(func).is_some() {
                    e.push(func);
                } else if resolver.path(func).is_none() {
                    o.push(func);
                }
            }
        }
        (e, o)
    };
    let list = start.elapsed();
    let roots = Roots { entries: &entries, on_demand: &on_demand };
    let setup = start.elapsed();
    if std::env::var_os("RUSTD_STATS").is_some() {
        eprintln!("entries: {:.1?}", setup - list);
    }
    // Not dropped from the roots: what they share with the others has to stay with rustc too.
    let mut denied = FxHashSet::default();
    if let Some(deny) = &args.deny {
        let deny = std::fs::read_to_string(deny)?;
        let deny: FxHashSet<&str> = deny.lines().collect();
        denied.extend(
            entries.iter().chain(&on_demand).copied().filter(|&func| {
                resolver.path(func).is_some_and(|path| deny.contains(path.as_str()))
            }),
        );
    }
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    // What every question about a crate's code comes down to first, asked one crate to a
    // thread: left to the questions, whichever gets there has the others wait for all of it.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let stale: Option<Vec<_>> = std::thread::scope(|scope| {
        let parts: Vec<_> = (0..threads)
            .map(|part| {
                let (db, known, local, next) = (db.clone(), &*known, &local, &next);
                scope.spawn(move || {
                    PROBING.set(true);
                    hir::attach_db(&db, || {
                        while let Some(&krate) =
                            local.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                        {
                            hir_ty::method_resolution::TraitImpls::for_crate(&db, krate.into());
                            hir_ty::method_resolution::InherentImpls::for_crate(&db, krate.into());
                        }
                        known.stale(&db, part, threads)
                    })
                })
            })
            .collect();
        parts.into_iter().map(|it| it.join().ok().flatten()).collect()
    });
    hir::attach_db(db, || known.set_stale(hir_db, stale.unwrap_or_default()));
    let looked = known.stale_steps();
    let revalidate = start.elapsed();
    // Salsa attaches the database for the duration of the closure; the solver needs it.
    let mut on_demand_rejections = Rejections::default();
    on_demand_rejections.patching = true;
    let active_known = if args.krate.is_some() { &mut *known } else { &mut on_demand_rejections };
    let unit = hir::attach_db(db, || {
        CodegenUnit::compile(hir_db, &resolver, roots, &denied, active_known)
    })
    .map_err(anyhow::Error::msg)?;
    if args.krate.is_none() {
        known.stats = on_demand_rejections.stats;
        if !is_cold {
            known.stats.patched = Some(unit.compiled.len());
        }
    }
    let name = |func| resolver.path(func).unwrap_or_else(|| "<assoc fn>".to_owned());
    let codegen = start.elapsed();
    if std::env::var_os("RUSTD_STATS").is_some() {
        eprintln!(
            "roots: {setup:.1?}, revalidate: {:.1?}, compile: {:.1?}",
            revalidate - setup,
            codegen - revalidate
        );
    }
    let start = Instant::now();
    if args.krate.is_none() && !is_cold {
        if let (Some(path), Some(object)) = (&args.patch, &unit.object) {
            let tmp = path.with_extension("o.tmp");
            std::fs::write(&tmp, object)?;
            std::fs::rename(&tmp, path)?;
        }
    } else {
        if let Some(object) = &unit.object {
            std::fs::write(&args.out, object)?;
            if let Some(patch_path) = &args.patch
                && !args.target.as_deref().is_some_and(|t| t.starts_with("wasm32"))
            {
                inject_hotpatch_rt(&args.out, patch_path, &args.root()?)?;
            }
        }
        if let (Some(path), Some(patch)) = (&args.patch, unit.patch.as_ref().or(unit.object.as_ref())) {
            let tmp = path.with_extension("o.tmp");
            std::fs::write(&tmp, patch)?;
            std::fs::rename(&tmp, path)?;
        }
    }
    let build = Build {
        compiled: unit.compiled.len(),
        skipped: unit.skipped.into_iter().map(|(func, e)| (name(func), e)).collect(),
        instances: unit.instances,
        codegen,
        emit: start.elapsed(),
        steps: [setup, revalidate - setup, codegen - revalidate],
        looked,
    };
    if let Some(path) = &args.report {
        let compiled: Vec<String> = unit.compiled.iter().map(|&(func, _)| name(func)).collect();
        build.write_report(path, &compiled)?;
    }
    if let Some(dir) = &args.isolate {
        std::fs::create_dir_all(dir)?;
        let mut index = String::new();
        for (i, &(func, _)) in unit.compiled.iter().enumerate() {
            let single = hir::attach_db(db, || {
                let roots = Roots { entries: &[func], on_demand: &[] };
                CodegenUnit::compile(hir_db, &resolver, roots, &Default::default(), known)
            })
            .map_err(anyhow::Error::msg)?;
            std::fs::write(dir.join(format!("{i}.o")), single.object.unwrap_or_default())?;
            index.push_str(&format!("{i}\t{}\n", name(func)));
        }
        std::fs::write(dir.join("index.tsv"), index)?;
    }
    Ok(build)
}

impl Build {
    fn write_report(&self, path: &Path, compiled: &[String]) -> anyhow::Result<()> {
        use std::fmt::Write;
        let mut out = String::new();
        writeln!(
            out,
            "#\tinstances\t{}\tcodegen_us\t{}",
            self.instances,
            self.codegen.as_micros()
        )?;
        for name in compiled {
            writeln!(out, "compiled\t{name}\t")?;
        }
        for (name, error) in &self.skipped {
            let error = error.to_string().replace(['\t', '\n'], " ");
            writeln!(out, "skipped\t{name}\t{error}")?;
        }
        Ok(std::fs::write(path, out)?)
    }
}

fn report(build: &Build) {
    eprintln!(
        "compiled {} of {} fns (+{} instances) in {:.1?} (+{:.1?} emit)",
        build.compiled,
        build.compiled + build.skipped.len(),
        build.instances,
        build.codegen,
        build.emit,
    );
    if let Some(path) = std::env::var_os("RUSTD_SKIPPED") {
        let all: String =
            build.skipped.iter().map(|(name, error)| format!("{name}: {error}\n")).collect();
        if let Err(error) = std::fs::write(&path, all) {
            eprintln!("rustd: cannot write {path:?}: {error}");
        }
    }
    if build.skipped.len() <= 8 {
        for (name, error) in &build.skipped {
            eprintln!("  left to rustc: {name}: {error}");
        }
        return;
    }
    // Bucket by the innermost reason, without the payload.
    let mut reasons: FxHashMap<String, (usize, &str)> = FxHashMap::default();
    for (name, error) in &build.skipped {
        let error = error.to_string();
        let reason = error.rsplit("`: ").next().unwrap_or(&error);
        let reason = reason.split(['(', '{']).next().unwrap_or(reason).trim();
        let entry = reasons.entry(reason.to_owned()).or_insert((0, name));
        entry.0 += 1;
    }
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by_key(|(_, (count, _))| std::cmp::Reverse(*count));
    for (reason, (count, example)) in reasons {
        eprintln!("  {count:>5}  {reason}  (e.g. {example})");
    }
}

fn link(args: &Args) -> anyhow::Result<()> {
    let Some(cmd) = &args.link_cmd else { return Ok(()) };
    let start = Instant::now();
    let status = Command::new("sh").arg("-c").arg(cmd).status()?;
    eprintln!("link: {:.1?} ({status})", start.elapsed());
    anyhow::ensure!(status.success(), "the link failed ({status})");
    Ok(())
}

/// Why the source as it is can't be built by the daemon, when it can't.
fn refusal(
    db: &RootDatabase,
    sources: &[vfs::FileId],
    paths: &FxHashMap<vfs::FileId, PathBuf>,
    out_of_date: Option<(FunctionId, String)>,
) -> Option<String> {
    for &file in sources {
        let edition = ide_db::base_db::relevant_crates(db, file)
            .first()
            .map_or(syntax::Edition::CURRENT, |it| it.data(db).edition);
        let parse = ide_db::base_db::EditionedFileId::new(db, file, edition).parse(db);
        if let Some(error) = parse.errors().first() {
            let path = paths.get(&file).map(|it| it.display().to_string()).unwrap_or_default();
            let line = db.file_text(file).text(db)[..usize::from(error.range().start())]
                .matches('\n')
                .count();
            return Some(format!("{path}:{}: {error}", line + 1));
        }
    }
    let (func, why) = out_of_date?;
    let name = hir::Function::from(func).name(db).display(db, syntax::Edition::CURRENT).to_string();
    // What the frontend turns down is as a rule code that does not compile.
    if why.starts_with("frontend:") {
        return Some(format!("`{name}` has errors, `cargo check` shows them: {why}"));
    }
    Some(format!(
        "`{name}` is not compiled by rustd and rustc's copy is from before the edit, \
         build with cargo: {why}"
    ))
}

#[derive(Clone, PartialEq, Eq)]
struct AdtSnapshot {
    id: hir_def::AdtId,
    size: u64,
    align: u64,
    fields: Vec<(String, Option<u64>)>,
    variants: usize,
}

#[derive(Clone, Default)]
struct FileSnapshot {
    adts: FxHashMap<String, AdtSnapshot>,
    consts: FxHashMap<String, (u64, Option<StaticId>)>,
}

fn snapshot_file_adts(db: &RootDatabase, file: vfs::FileId) -> FileSnapshot {
    use std::hash::{Hash, Hasher};
    let mut adts = FxHashMap::default();
    let mut consts = FxHashMap::default();
    let sema = hir::Semantics::new(db);
    let mut modules: Vec<hir::Module> = sema.file_to_module_defs(file).collect();
    let mut seen_mods = FxHashSet::default();
    while let Some(module) = modules.pop() {
        let mod_id = hir_def::ModuleId::from(module);
        if !seen_mods.insert(mod_id) {
            continue;
        }
        let scope = mod_id.def_map(db).module(db, mod_id).scope(db);
        for decl in scope.declarations() {
            match decl {
                ModuleDefId::ModuleId(child) => {
                    let child_mod = hir::Module::from(child);
                    if child_mod
                        .definition_source_file_id(db)
                        .original_file_respecting_includes(db)
                        .file_id(db)
                        == file
                    {
                        modules.push(child_mod);
                    }
                }
                ModuleDefId::ConstId(c) => {
                    if let Some(name) = hir::Const::from(c)
                        .name(db)
                        .map(|n| n.display(db, syntax::Edition::CURRENT).to_string())
                    {
                        let def: DefWithBodyId = c.into();
                        let mut hasher = rustc_hash::FxHasher::default();
                        Body::of(db, def)
                            .pretty_print(db, def, syntax::Edition::CURRENT)
                            .hash(&mut hasher);
                        consts.insert(name, (hasher.finish(), None));
                    }
                }
                ModuleDefId::StaticId(s) => {
                    let name = hir::Static::from(s)
                        .name(db)
                        .display(db, syntax::Edition::CURRENT)
                        .to_string();
                    let def: DefWithBodyId = s.into();
                    let mut hasher = rustc_hash::FxHasher::default();
                    Body::of(db, def)
                        .pretty_print(db, def, syntax::Edition::CURRENT)
                        .hash(&mut hasher);
                    consts.insert(name, (hasher.finish(), Some(s)));
                }
                _ => {}
            }
        }
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                if let AssocItemId::ConstId(c) = item
                    && let Some(name) = hir::Const::from(c)
                        .name(db)
                        .map(|n| n.display(db, syntax::Edition::CURRENT).to_string())
                {
                    let def: DefWithBodyId = c.into();
                    let mut hasher = rustc_hash::FxHasher::default();
                    Body::of(db, def)
                        .pretty_print(db, def, syntax::Edition::CURRENT)
                        .hash(&mut hasher);
                    consts.insert(name, (hasher.finish(), None));
                }
            }
        }
        for decl in module.declarations(db) {
            let ModuleDef::Adt(adt) = decl else { continue };
            let name = adt.name(db).display(db, syntax::Edition::CURRENT).to_string();
            let layout = adt.layout(db).ok();
            let (fields, variants) = match adt {
                hir::Adt::Struct(s) => (
                    s.fields(db)
                        .into_iter()
                        .map(|f| {
                            (
                                f.name(db).display(db, syntax::Edition::CURRENT).to_string(),
                                layout.as_ref().and_then(|l| l.field_offset(f)),
                            )
                        })
                        .collect(),
                    1,
                ),
                hir::Adt::Union(u) => (
                    u.fields(db)
                        .into_iter()
                        .map(|f| {
                            (
                                f.name(db).display(db, syntax::Edition::CURRENT).to_string(),
                                layout.as_ref().and_then(|l| l.field_offset(f)),
                            )
                        })
                        .collect(),
                    1,
                ),
                hir::Adt::Enum(e) => (
                    vec![(
                        "tag".to_owned(),
                        layout.as_ref().and_then(|l| l.enum_tag_size().map(|s| s as u64)),
                    )],
                    e.variants(db).len(),
                ),
            };
            adts.insert(
                name,
                AdtSnapshot {
                    id: hir_def::AdtId::from(adt),
                    size: layout.as_ref().map_or(0, |l| l.size()),
                    align: layout.as_ref().map_or(0, |l| l.align()),
                    fields,
                    variants,
                },
            );
        }
    }
    FileSnapshot { adts, consts }
}

fn detect_adt_layout_warnings(
    db: &RootDatabase,
    dirty_files: &FxHashSet<vfs::FileId>,
    initial_adts: &FxHashMap<vfs::FileId, FileSnapshot>,
) -> (Vec<String>, FxHashSet<hir_def::AdtId>, FxHashSet<String>) {
    let mut warnings = Vec::new();
    let mut changed_adts = FxHashSet::default();
    let mut changed_consts = FxHashSet::default();
    let mut edited_statics = FxHashSet::default();
    hir::attach_db(db, || {
        for &file in dirty_files {
            let Some(orig_snap) = initial_adts.get(&file) else { continue };
            let cur_snap = snapshot_file_adts(db, file);
            for (name, orig) in &orig_snap.adts {
                if let Some(cur) = cur_snap.adts.get(name)
                    && (cur.size != orig.size
                        || cur.align != orig.align
                        || cur.fields != orig.fields
                        || cur.variants != orig.variants)
                {
                    changed_adts.insert(cur.id);
                    warnings.push(format!(
                        "layout of `{name}` changed ({}B/align {} -> {}B/align {}); \
                         recompiling crate callers (live in-memory instances still use old layout)",
                        orig.size, orig.align, cur.size, cur.align
                    ));
                }
            }
            for (name, &(orig_h, _)) in &orig_snap.consts {
                if let Some(&(cur_h, cur_stat)) = cur_snap.consts.get(name)
                    && cur_h != orig_h
                {
                    changed_consts.insert(name.clone());
                    if let Some(s) = cur_stat {
                        edited_statics.insert(s);
                    }
                }
            }
        }
    });
    if let Ok(mut guard) = hir_ty::mir::codegen::EDITED_STATICS.lock() {
        *guard = (!edited_statics.is_empty()).then_some(edited_statics);
    }
    (warnings, changed_adts, changed_consts)
}

fn modules_in_file(db: &RootDatabase, file: vfs::FileId) -> Vec<hir::Module> {
    let sema = hir::Semantics::new(db);
    let mut stack: Vec<hir::Module> = sema.file_to_module_defs(file).collect();
    let mut seen = FxHashSet::default();
    let mut out = Vec::new();
    while let Some(module) = stack.pop() {
        let mod_id = hir_def::ModuleId::from(module);
        if !seen.insert(mod_id) {
            continue;
        }
        out.push(module);
        let scope = mod_id.def_map(db).module(db, mod_id).scope(db);
        for decl in scope.declarations() {
            if let ModuleDefId::ModuleId(child) = decl {
                let child_mod = hir::Module::from(child);
                if child_mod
                    .definition_source_file_id(db)
                    .original_file_respecting_includes(db)
                    .file_id(db)
                    == file
                {
                    stack.push(child_mod);
                }
            }
        }
    }
    out
}

#[derive(Clone, Default)]
struct CheckReport {
    errors: usize,
    warnings: usize,
    rendered: String,
    first_summary: Option<String>,
}

fn format_cargo_snippet(
    db: &RootDatabase,
    paths: &FxHashMap<vfs::FileId, PathBuf>,
    root: &Path,
    file_id: vfs::FileId,
    range: syntax::TextRange,
    code: Option<&'static str>,
    message: &str,
    label: &str,
) -> String {
    let full_path = paths.get(&file_id).cloned().unwrap_or_default();
    let rel_path = full_path
        .strip_prefix(root)
        .unwrap_or(&full_path)
        .display()
        .to_string();
    let text = db.file_text(file_id).text(db);
    let start_off = usize::from(range.start()).min(text.len());
    let end_off = usize::from(range.end()).min(text.len()).max(start_off);
    let before = &text[..start_off];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[start_off..].find('\n').map_or(text.len(), |i| start_off + i);
    let line_no = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let raw_line = text[line_start..line_end].trim_end_matches('\r');
    let span_start_in_line = start_off.saturating_sub(line_start).min(raw_line.len());
    let span_end_in_line = end_off.min(line_end).saturating_sub(line_start).min(raw_line.len());
    let mut display_line = String::with_capacity(raw_line.len() + 16);
    let mut caret_col = 0usize;
    let mut caret_len = 0usize;
    for (byte_idx, ch) in raw_line.char_indices() {
        let width = if ch == '\t' { 4 } else { 1 };
        if byte_idx < span_start_in_line {
            caret_col += width;
        } else if byte_idx < span_end_in_line {
            caret_len += width;
        }
        if ch == '\t' {
            display_line.push_str("    ");
        } else {
            display_line.push(ch);
        }
    }
    let col_no = raw_line[..span_start_in_line].chars().count() + 1;
    let carets = "^".repeat(caret_len.max(1));
    let pad_num = line_no.to_string();
    let gutter_pad = " ".repeat(pad_num.len());
    let caret_pad = " ".repeat(caret_col);
    let header = match code {
        Some(c) => format!("\x1b[1;31merror[{c}]\x1b[0m\x1b[1m: {message}\x1b[0m"),
        None => format!("\x1b[1;31merror\x1b[0m\x1b[1m: {message}\x1b[0m"),
    };
    format!(
        "{header}\n\
         {gutter_pad}\x1b[1;34m-->\x1b[0m {rel_path}:{line_no}:{col_no}\n\
         {gutter_pad} \x1b[1;34m|\x1b[0m\n\
         \x1b[1;34m{pad_num} |\x1b[0m {display_line}\n\
         {gutter_pad} \x1b[1;34m|\x1b[0m {caret_pad}\x1b[1;31m{carets} {label}\x1b[0m\n\n"
    )
}

fn check_files_diagnostics(
    db: &RootDatabase,
    files_to_check: &[vfs::FileId],
    layout_warnings: &[String],
    paths: &FxHashMap<vfs::FileId, PathBuf>,
    root: &Path,
) -> CheckReport {
    use hir::AnyDiagnostic as Diagnostic;
    let mut rep = CheckReport::default();
    let mut seen_codes = Vec::new();
    let mut seen_diags = FxHashSet::default();
    for w in layout_warnings {
        rep.warnings += 1;
        rep.rendered.push_str(&format!("\x1b[1;33mwarning\x1b[0m\x1b[1m: {w}\x1b[0m\n\n"));
    }
    hir::attach_db(db, || {
        let sema = hir::Semantics::new(db);
        for &file in files_to_check {
            let crates = ide_db::base_db::relevant_crates(db, file);
            let edition = crates
                .first()
                .map_or(syntax::Edition::CURRENT, |&krate| Crate::from(krate).edition(db));
            let parse = ide_db::base_db::EditionedFileId::new(db, file, edition).parse(db);
            for err in parse.errors() {
                let msg = err.to_string();
                if seen_diags.insert((file, err.range(), msg.clone())) {
                    rep.errors += 1;
                    if rep.first_summary.is_none() {
                        rep.first_summary = Some(msg.clone());
                    }
                    rep.rendered.push_str(&format_cargo_snippet(
                        db,
                        paths,
                        root,
                        file,
                        err.range(),
                        None,
                        &msg,
                        "syntax error",
                    ));
                }
            }
            for module in modules_in_file(db, file) {
                let mut found = Vec::new();
                module.diagnostics(db, &mut found, false);
                let disp = module.krate(db).to_display_target(db);
                for diag in &found {
                    let info: Option<(
                        Option<&'static str>,
                        String,
                        hir::FileRangeWrapper<vfs::FileId>,
                        String,
                        &'static str,
                    )> = match diag {
                        Diagnostic::TypeMismatch(d) => {
                            let exp = d.expected.display(db, disp).to_string();
                            let act = d.actual.display(db, disp).to_string();
                            let r = sema.diagnostics_display_range(d.expr_or_pat.map(|it| it.into()));
                            Some((
                                Some("E0308"),
                                "mismatched types".to_owned(),
                                r,
                                format!("expected `{exp}`, found `{act}`"),
                                "mismatched types",
                            ))
                        }
                        Diagnostic::UnresolvedIdent(d) => {
                            let r = match d.node.value.1 {
                                Some(sub) => sema.diagnostics_display_range_for_range(
                                    hir::InFile::new(d.node.file_id, sub),
                                ),
                                None => sema.diagnostics_display_range(hir::InFile::new(
                                    d.node.file_id,
                                    d.node.value.0.into(),
                                )),
                            };
                            let txt = db.file_text(r.file_id).text(db);
                            let s = usize::from(r.range.start()).min(txt.len());
                            let e = usize::from(r.range.end()).min(txt.len()).max(s);
                            let ident = &txt[s..e];
                            Some((
                                Some("E0425"),
                                format!("cannot find value `{ident}` in this scope"),
                                r,
                                "not found in this scope".to_owned(),
                                "unresolved name",
                            ))
                        }
                        Diagnostic::MismatchedArgCount(d) => {
                            let r = sema.diagnostics_display_range(d.call_expr.map(|it| it.into()));
                            Some((
                                Some("E0061"),
                                format!(
                                    "this function takes {} argument(s) but {} argument(s) were supplied",
                                    d.expected, d.found
                                ),
                                r,
                                format!("expected {} argument(s), found {}", d.expected, d.found),
                                "wrong number of arguments",
                            ))
                        }
                        Diagnostic::MismatchedTupleStructPatArgCount(d) => {
                            let r = sema.diagnostics_display_range(d.expr_or_pat.map(|it| it.into()));
                            Some((
                                Some("E0023"),
                                format!(
                                    "this pattern has {} field(s), but the corresponding tuple variant has {} field(s)",
                                    d.found, d.expected
                                ),
                                r,
                                format!("expected {} field(s), found {}", d.expected, d.found),
                                "wrong number of arguments",
                            ))
                        }
                        Diagnostic::MissingFields(d) => {
                            let names: Vec<String> = d
                                .missed_fields
                                .iter()
                                .map(|(n, _)| format!("`{}`", n.display(db, edition)))
                                .collect();
                            let list = names.join(", ");
                            let r = sema.diagnostics_display_range(hir::InFile::new(
                                d.file,
                                d.field_list_parent.into(),
                            ));
                            Some((
                                Some("E0063"),
                                format!("missing field(s) {list} in initializer"),
                                r,
                                format!("missing {list}"),
                                "missing fields",
                            ))
                        }
                        Diagnostic::NoSuchField(d) => {
                            let r = sema.diagnostics_display_range(d.field.map(|it| it.into()));
                            Some((
                                Some("E0560"),
                                "struct or enum variant has no such field".to_owned(),
                                r,
                                "unknown field".to_owned(),
                                "no such field",
                            ))
                        }
                        Diagnostic::UnresolvedField(d) => {
                            let recv = d.receiver.display(db, disp).to_string();
                            let name = d.name.display(db, edition).to_string();
                            let r = sema.diagnostics_display_range(d.expr.map(|it| it.into()));
                            Some((
                                Some("E0609"),
                                format!("no field `{name}` on type `{recv}`"),
                                r,
                                "unknown field".to_owned(),
                                "no such field",
                            ))
                        }
                        Diagnostic::UnresolvedMethodCall(d) => {
                            let recv = d.receiver.display(db, disp).to_string();
                            let name = d.name.display(db, edition).to_string();
                            let r = sema.diagnostics_display_range(d.expr.map(|it| it.into()));
                            Some((
                                Some("E0599"),
                                format!("no method named `{name}` found for type `{recv}` in the current scope"),
                                r,
                                format!("method not found in `{recv}`"),
                                "no such method",
                            ))
                        }
                        Diagnostic::UnresolvedAssocItem(d) => {
                            let r = sema.diagnostics_display_range(d.expr_or_pat.map(|it| it.into()));
                            Some((
                                Some("E0599"),
                                "no associated item found for type in the current scope".to_owned(),
                                r,
                                "associated item not found".to_owned(),
                                "no such method",
                            ))
                        }
                        Diagnostic::MissingMatchArms(d) => {
                            let r = sema.diagnostics_display_range(d.scrutinee_expr.map(|it| it.into()));
                            Some((
                                Some("E0004"),
                                format!("non-exhaustive patterns: {} not covered", d.uncovered_patterns),
                                r,
                                format!("pattern {} not covered", d.uncovered_patterns),
                                "patterns not covered",
                            ))
                        }
                        Diagnostic::NonExhaustiveLet(d) => {
                            let r = sema.diagnostics_display_range(d.pat.map(|it| it.into()));
                            Some((
                                Some("E0005"),
                                format!("refutable pattern in local binding: {} not covered", d.uncovered_patterns),
                                r,
                                format!("pattern {} not covered", d.uncovered_patterns),
                                "patterns not covered",
                            ))
                        }
                        Diagnostic::TraitImplMissingAssocItems(d) => {
                            let names: Vec<String> = d
                                .missing
                                .iter()
                                .map(|(n, _)| format!("`{}`", n.display(db, edition)))
                                .collect();
                            let list = names.join(", ");
                            let r = sema.diagnostics_display_range(hir::InFile::new(
                                d.file_id,
                                d.impl_.into(),
                            ));
                            Some((
                                Some("E0046"),
                                format!("not all trait items implemented, missing: {list}"),
                                r,
                                format!("missing {list} in implementation"),
                                "an impl does not match its trait",
                            ))
                        }
                        Diagnostic::TraitImplRedundantAssocItems(d) => {
                            let item_name = d.assoc_item.0.display(db, edition).to_string();
                            let trait_name = d.trait_.name(db).display(db, edition).to_string();
                            let r = sema.diagnostics_display_range(hir::InFile::new(
                                d.file_id,
                                d.impl_.into(),
                            ));
                            Some((
                                Some("E0407"),
                                format!("item `{item_name}` is not a member of trait `{trait_name}`"),
                                r,
                                format!("not a member of trait `{trait_name}`"),
                                "an impl does not match its trait",
                            ))
                        }
                        Diagnostic::UnresolvedModule(d) => {
                            let r = sema.diagnostics_display_range(d.decl.map(|it| it.into()));
                            Some((
                                Some("E0583"),
                                "file not found for module".to_owned(),
                                r,
                                "file not found for module".to_owned(),
                                "a module without a file",
                            ))
                        }
                        Diagnostic::UnresolvedImport(d) => {
                            let r = sema.diagnostics_display_range(d.decl.map(|it| it.into()));
                            Some((
                                Some("E0432"),
                                "unresolved import".to_owned(),
                                r,
                                "unresolved import".to_owned(),
                                "unresolved import",
                            ))
                        }
                        Diagnostic::UnresolvedMacroCall(d) => {
                            let name = d.path.display(db, edition).to_string();
                            let r = sema.diagnostics_display_range_for_range(d.range);
                            Some((
                                None,
                                format!("cannot find macro `{name}` in this scope"),
                                r,
                                "not found in this scope".to_owned(),
                                "unresolved macro",
                            ))
                        }
                        _ => None,
                    };
                    if let Some((code, msg, range, label, short_why)) = info
                        && seen_diags.insert((range.file_id, range.range, msg.clone()))
                    {
                        rep.errors += 1;
                        if let Some(c) = code
                            && !seen_codes.contains(&c)
                        {
                            seen_codes.push(c);
                        }
                        if rep.first_summary.is_none() {
                            let mod_name = module.name(db).map_or_else(
                                || "the crate root".to_owned(),
                                |it| format!("`{}`", it.display(db, syntax::Edition::CURRENT)),
                            );
                            rep.first_summary = Some(format!(
                                "{mod_name} has errors, `cargo check` shows them: {short_why}"
                            ));
                        }
                        rep.rendered.push_str(&format_cargo_snippet(
                            db,
                            paths,
                            root,
                            range.file_id,
                            range.range,
                            code,
                            &msg,
                            &label,
                        ));
                    }
                }
            }
        }
    });
    if let Some(&first_code) = seen_codes.first() {
        rep.rendered.push_str(&format!(
            "\x1b[1mFor more information about this error, try `rustc --explain {first_code}`.\x1b[0m\n"
        ));
    }
    rep
}

/// A file whose changes the daemon picks up.
enum Watched {
    Source(vfs::FileId),
    /// Named by an `include_bytes!` or `include_str!`, and not a source file.
    Included(AbsPathBuf),
}

struct WatchedFile {
    file: Watched,
    path: PathBuf,
    mtime: SystemTime,
}

impl WatchedFile {
    fn new(file: Watched, path: &Path) -> Option<WatchedFile> {
        // Everything but the sysroot and the registry: path dependencies live outside the project.
        let immutable = path
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some("target" | "rustlib" | "registry")));
        if immutable {
            return None;
        }
        let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
        Some(WatchedFile { file, path: path.to_owned(), mtime })
    }

    fn sources(vfs: &Vfs) -> Vec<WatchedFile> {
        vfs.iter()
            .filter_map(|(id, path)| {
                WatchedFile::new(Watched::Source(id), path.as_path()?.as_ref())
            })
            .collect()
    }

    /// Adds the files the last build included that are not watched yet. One is read again once
    /// its timestamp is taken, so a write between the build's read and now is not lost: the
    /// result is whether there was one.
    fn watch_included(files: &mut Vec<WatchedFile>, db: &mut RootDatabase) -> bool {
        let watched: FxHashSet<&Path> = files.iter().map(|it| &*it.path).collect();
        let new: Vec<AbsPathBuf> = db
            .included_files()
            .into_iter()
            .filter(|it| !watched.contains(AsRef::<Path>::as_ref(it)))
            .collect();
        let mut changed = false;
        for path in new {
            if let Some(file) = WatchedFile::new(Watched::Included(path.clone()), path.as_ref()) {
                files.push(file);
                changed |= db.reload_included_file(&path);
            }
        }
        changed
    }
}

/// Compiles every crate asked for, each as it would be alone.
/// Builds each crate into `<out>/<crate>.o`. The crates are separate programs that share only
/// the workspace, so each one sees the symbols of its own rlib and of the rlibs that belong to
/// none of them: a symbol from another crate's rlib would not be there at link time.
fn build_batch(db: &RootDatabase, args: Args) -> anyhow::Result<()> {
    let (out, report) = (args.out.clone(), args.report.clone().context("a batch needs --report")?);
    let first_krate = args.krate.clone().context("a batch needs --crate")?;
    let crates: Vec<_> =
        std::iter::once(first_krate).chain(args.batch.iter().cloned()).collect();
    let rlib_of = |file: &Path| {
        let name = file.file_name()?.to_str()?.strip_prefix("lib")?;
        let name = name.rsplit_once('-').map_or(name, |(name, _)| name);
        crates.iter().find(|krate| krate.as_str() == name)
    };
    for krate in &crates {
        let symbol_files: Vec<_> = args
            .symbol_files
            .iter()
            .filter(|file| rlib_of(file).is_none_or(|owner| owner == krate))
            .cloned()
            .collect();
        let table = &SymbolTable::load(&symbol_files, &args.root()?)?;
        let args = Args {
            project: args.project.clone(),
            krate: Some(krate.clone()),
            batch: Vec::new(),
            out: out.join(format!("{krate}.o")),
            symbol_files: Vec::new(),
            watch: false,
            socket: None,
            link_cmd: None,
            patch: None,
            report: Some(report.join(format!("{krate}.tsv"))),
            deny: None,
            isolate: None,
            cfg_test: args.cfg_test,
            features: args.features.clone(),
            target: args.target.clone(),
        };
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build(
                db,
                table,
                &args,
                &mut Rejections::default(),
                &mut Listed::default(),
                &FxHashSet::default(),
                &FxHashMap::default(),
                &FxHashSet::default(),
                &FxHashSet::default(),
                true,
            )
        }));
        let error = match built {
            Ok(Ok(_)) => continue,
            Ok(Err(error)) => format!("error: {error:#}"),
            Err(panic) => {
                let message = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("panic");
                format!("panicked: {message}")
            }
        };
        std::fs::write(report.join(format!("{krate}.error")), error)?;
    }
    Ok(())
}

/// What a client asks of the daemon, as the one line it sends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Request {
    /// Bring the objects and the linked program up to date with the files on disk.
    Sync,
    /// Bring the objects up to date and leave the linking to the client, which loads the
    /// patch object into a program that is running.
    Patch,
    /// Run incremental syntax and semantic diagnostics and write a cargo-style report.
    Check,
}

impl Request {
    fn parse(line: &str) -> Option<Request> {
        match line {
            "sync" => Some(Request::Sync),
            "patch" => Some(Request::Patch),
            "check" => Some(Request::Check),
            _ => None,
        }
    }

    fn line(self) -> &'static str {
        match self {
            Request::Sync => "sync\n",
            Request::Patch => "patch\n",
            Request::Check => "check\n",
        }
    }
}

/// Sends `request` to the daemon listening on `socket` and waits for it. The answer is one
/// line: `ok unchanged`, or `ok patch=PATH ...` when the object at PATH holds the functions that
/// differ from the first build's, or `ok object=PATH ...` when the whole object was written
/// again.
fn ask(request: Request, socket: &Path) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("no daemon at {}", socket.display()))?;
    stream.write_all(request.line().as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let reply = reply.trim_end();
    anyhow::ensure!(reply.starts_with("ok"), "daemon: {reply}");
    println!("{reply}");
    Ok(())
}

fn inject_hotpatch_rt(base_o: &Path, patch_o: &Path, root: &Path) -> anyhow::Result<()> {
    let rt_o = base_o.with_extension("rt.o");
    let cave_o = base_o.with_extension("cave.o");
    if let Ok(rt_m) = std::fs::metadata(&rt_o).and_then(|m| m.modified())
        && cave_o.exists()
        && let Ok(exe_m) = std::env::current_exe()
            .and_then(|p| std::fs::metadata(p))
            .and_then(|m| m.modified())
        && rt_m >= exe_m
    {
        return Ok(());
    }
    let rt_src = include_str!("hotpatch_rt.rs");
    let rt_rs = base_o.with_extension("rt.rs");
    std::fs::write(&rt_rs, rt_src)?;
    let abs_patch = std::env::current_dir()?.join(patch_o);
    let status = Command::new("rustc")
        .current_dir(root)
        .args([
            "--edition=2024",
            "--crate-name=rustd_hotpatch_rt",
            "--crate-type=rlib",
            "--emit=obj",
            "-O",
        ])
        .arg(&rt_rs)
        .arg("-o")
        .arg(&rt_o)
        .env("RUSTD_DEFAULT_PATCH_PATH", &abs_patch)
        .status()
        .context("compiling hotpatch_rt.rs")?;
    anyhow::ensure!(status.success(), "rustc failed on hotpatch_rt.rs ({status})");
    let _ = std::fs::remove_file(&rt_rs);

    let cave_rs = base_o.with_extension("cave.rs");
    std::fs::write(
        &cave_rs,
        "#[repr(C, align(1))] pub struct Cave([u8; 81920]);\n\
         #[used]\n\
         #[unsafe(no_mangle)]\n\
         #[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__TEXT,__text\"))]\n\
         #[cfg_attr(target_os = \"linux\", unsafe(link_section = \".text\"))]\n\
         pub static __rustd_veneer_cave_end: Cave = Cave([0; 81920]);\n",
    )?;
    let cave_status = Command::new("rustc")
        .current_dir(root)
        .args([
            "--edition=2024",
            "--crate-name=rustd_veneer_cave",
            "--crate-type=rlib",
            "--emit=obj",
            "-C",
            "panic=abort",
            "-O",
        ])
        .arg(&cave_rs)
        .arg("-o")
        .arg(&cave_o)
        .status()
        .context("compiling cave.rs")?;
    anyhow::ensure!(cave_status.success(), "rustc failed on cave.rs ({cave_status})");
    let _ = std::fs::remove_file(&cave_rs);
    Ok(())
}

/// Waits for a build request on `listener`. The result is where to send the outcome.
fn requested(listener: &UnixListener, watch: bool) -> anyhow::Result<Option<(Request, UnixStream)>> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request)?;
                if let Some(request) = Request::parse(request.trim_end()) {
                    return Ok(Some((request, stream)));
                }
                let _ = writeln!(&stream, "error: unknown request {:?}", request.trim_end());
            }
            Err(e) if watch && e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn main() -> anyhow::Result<()> {
    if let Some((request, socket)) =
        std::env::args().nth(1).and_then(|it| Request::parse(&it)).zip(std::env::args().nth(2))
        && Path::new(&socket).extension().is_some_and(|e| e == "sock")
    {
        return ask(request, Path::new(&socket));
    }
    if let Some(first) = std::env::args().nth(1)
        && cli::is_subcommand(&first)
    {
        return cli::dispatch(&first, std::env::args().skip(2).collect());
    }
    let args = Args::parse()?;
    if args.target.as_deref().is_some_and(|t| t.starts_with("wasm32")) {
        hir_ty::mir::codegen::WASM32_TARGET.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    hir_def::nameres::EXPAND_BUILTIN_DERIVES.store(true, std::sync::atomic::Ordering::Relaxed);
    project_model::SYSROOT_AS_BUILT.store(true, std::sync::atomic::Ordering::Relaxed);
    // Nothing asks for a symbol before the first build, so they load next to the workspace.
    let (symbol_files, root) = (args.symbol_files.clone(), args.root()?);
    let (cache_fp, baseline_fp) = {
        use std::hash::{Hash, Hasher};
        let mut bh = rustc_hash::FxHasher::default();
        for f in &symbol_files {
            f.hash(&mut bh);
            if let Ok(m) = std::fs::metadata(f) {
                m.len().hash(&mut bh);
                if let Ok(t) = m.modified()
                    && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
                {
                    d.as_nanos().hash(&mut bh);
                }
            }
        }
        for mf in [&args.project, &root.join("Cargo.lock")] {
            if let Ok(m) = std::fs::metadata(mf) {
                m.len().hash(&mut bh);
                if let Ok(t) = m.modified()
                    && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
                {
                    d.as_nanos().hash(&mut bh);
                }
            }
        }
        args.cfg_test.hash(&mut bh);
        args.target.hash(&mut bh);
        args.features.hash(&mut bh);
        std::env::var("RUSTD_PKG").ok().hash(&mut bh);
        let b_fp = bh.finish();
        let mut h = rustc_hash::FxHasher::default();
        b_fp.hash(&mut h);
        SymbolTable::files_fingerprint(&symbol_files).hash(&mut h);
        (format!("{:016x}", h.finish()), format!("{:016x}", b_fp))
    };
    if !args.out.is_dir() {
        unsafe {
            std::env::set_var("RUSTD_CACHE_FP", &cache_fp);
            std::env::set_var("RUSTD_CACHE_META", args.out.with_extension("meta.cache"));
            std::env::set_var("RUSTD_CACHE_TREE", args.out.with_extension("tree.cache"));
            std::env::set_var("RUSTD_CACHE_BS", args.out.with_extension("bs.cache"));
        }
    }
    let baseline_cache_path =
        (!args.out.is_dir()).then(|| args.out.with_extension("baseline.cache"));
    let sym_cache_path = (!args.out.is_dir()).then(|| args.out.with_extension("symbols.cache"));
    let table = std::thread::spawn(move || {
        let start = Instant::now();
        let table = SymbolTable::load_cached(&symbol_files, &root, sym_cache_path.as_deref());
        if let Ok(table) = &table {
            eprintln!("symbols: {} in {:.1?}", table.len(), start.elapsed());
        }
        table
    });

    let start = Instant::now();
    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        set_test: args.cfg_test,
        target: args.target.clone(),
        features: CargoFeatures::Selected {
            features: args.features.clone(),
            no_default_features: false,
        },
        ..Default::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };
    let (mut db, vfs, _proc_macro) = load(&args.project, &cargo_config, &load_config)?;
    eprintln!("load workspace: {:.1?}", start.elapsed());

    if args.out.is_dir() {
        return build_batch(&db, args);
    }

    let mut resolved_deps = false;
    if args.krate.is_some() || (args.socket.is_none() && !args.watch) {
        let start = Instant::now();
        let resolved = resolve_crates(&db, &args)?;
        eprintln!("resolve: {resolved} crates in {:.1?}", start.elapsed());
        resolved_deps = true;
    }

    anyhow::ensure!(args.patch.is_none() || args.isolate.is_none(), "--patch with --isolate");
    let report_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !PROBING.get() {
            report_panic(info);
        }
    }));
    let mut known = Rejections::default();
    known.patching = args.patch.is_some();
    let mut listed = Listed::default();
    let mut outlines = outline::Outlines::default();
    let mut initial_texts: FxHashMap<vfs::FileId, triomphe::Arc<str>> = FxHashMap::default();
    let mut initial_serials: FxHashMap<FunctionId, u64> = FxHashMap::default();
    let mut initial_adts: FxHashMap<vfs::FileId, FileSnapshot> = FxHashMap::default();
    let mut dirty_files: FxHashSet<vfs::FileId> = FxHashSet::default();
    let mut table = table.join().map_err(|_| anyhow::anyhow!("loading the symbols panicked"))??;
    let cold = build(
        &db,
        &table,
        &args,
        &mut known,
        &mut listed,
        &dirty_files,
        &initial_serials,
        &FxHashSet::default(),
        &FxHashSet::default(),
        true,
    )?;
    eprint!("cold: ");
    report(&cold);
    report_stats(&known.stats);
    let cold_bin_path = args.out.parent().and_then(|d| Some(d.join(d.file_name()?)));
    let skip_cold_link = args.krate.is_none()
        && cold_bin_path.as_ref().is_some_and(|bp| {
            let base_p = bp.with_extension("base");
            let rt_o = args.out.with_extension("rt.o");
            if let (Ok(bm), Ok(rm)) = (
                std::fs::metadata(&base_p).and_then(|m| m.modified()),
                std::fs::metadata(&rt_o).and_then(|m| m.modified()),
            ) {
                bm >= rm && bp.exists()
            } else {
                false
            }
        });
    if !skip_cold_link {
        link(&args)?;
    }
    let bin_index = cold_bin_path
        .filter(|p| p.exists())
        .and_then(|bin_path| {
            table.filter_to_binary(&bin_path);
            incremental_link::BinaryIndex::from_cold_binary(&bin_path)
        });
    hir::attach_db(&db, || known.out_of_date(&db));
    if args.socket.is_none() && !args.watch {
        return Ok(());
    }

    // The first change after a build of everything waits for what that build left behind.
    // Paid here, the first edit does not.
    {
        use ide_db::base_db::salsa::{Database, Durability};
        db.synthetic_write(Durability::LOW);
    }
    let mut files = WatchedFile::sources(&vfs);
    eprintln!("watching {} files", files.len());
    let mut baseline_by_path: FxHashMap<PathBuf, String> = FxHashMap::default();
    if let Some(bp) = &baseline_cache_path
        && let Ok(bytes) = std::fs::read(bp)
        && bytes.starts_with(b"RUSTDBAS")
    {
        let mut pos = 8usize;
        let get_u32 = |pos: &mut usize| -> Option<u32> {
            let v = u32::from_le_bytes(bytes.get(*pos..*pos + 4)?.try_into().ok()?);
            *pos += 4;
            Some(v)
        };
        let get_str = |pos: &mut usize| -> Option<String> {
            let len = get_u32(pos)? as usize;
            let s = std::str::from_utf8(bytes.get(*pos..*pos + len)?).ok()?.to_owned();
            *pos += len;
            Some(s)
        };
        if get_str(&mut pos).as_deref() == Some(baseline_fp.as_str())
            && let Some(count) = get_u32(&mut pos)
        {
            for _ in 0..count {
                if let (Some(p), Some(txt)) = (get_str(&mut pos), get_str(&mut pos)) {
                    baseline_by_path.insert(PathBuf::from(p), txt);
                }
            }
        }
    }
    let save_baseline = |bp: Option<&Path>, fp: &str, map: &FxHashMap<PathBuf, String>| {
        let Some(bp) = bp else { return };
        let mut out = Vec::with_capacity(64 * 1024);
        let put_str = |out: &mut Vec<u8>, s: &str| {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        };
        out.extend_from_slice(b"RUSTDBAS");
        put_str(&mut out, fp);
        out.extend_from_slice(&(map.len() as u32).to_le_bytes());
        for (k, v) in map {
            put_str(&mut out, &k.to_string_lossy());
            put_str(&mut out, v);
        }
        let _ = std::fs::write(bp, out);
    };
    let mut offline_restore = ChangeWithProcMacros::default();
    let mut has_offline_restore = false;
    for wf in &mut files {
        if let Watched::Source(id) = wf.file
            && let Some(orig_text) = baseline_by_path.get(&wf.path)
            && db.file_text(id).text(&db).as_ref() != orig_text.as_str()
        {
            offline_restore.change_file(id, Some(orig_text.clone()));
            wf.mtime = SystemTime::UNIX_EPOCH;
            has_offline_restore = true;
        }
    }
    if has_offline_restore {
        db.apply_change(offline_restore);
    }
    ide_db::executions::take(&db);
    let sources: Vec<_> = files
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some((id, &*it.path)),
            Watched::Included(_) => None,
        })
        .collect();
    let mut tree = tree::Tree::new(&db, vfs, &sources);
    let listener = match &args.socket {
        Some(path) => {
            // A socket file left by a daemon that is gone can't be bound again.
            let _ = std::fs::remove_file(path);
            let bind_path = std::env::current_dir()
                .ok()
                .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf))
                .unwrap_or_else(|| path.clone());
            let l = UnixListener::bind(&bind_path)?;
            if args.watch {
                l.set_nonblocking(true)?;
            }
            Some(l)
        }
        None => None,
    };
    let mut unprimed_crates: Vec<Crate> = if args.krate.is_none() {
        let bin_crate = std::env::var("RUSTD_BIN_CRATE").ok();
        Crate::all(&db)
            .into_iter()
            .filter(|it| {
                it.origin(&db).is_local()
                    && bin_crate.as_deref().is_some_and(|bc| {
                        it.display_name(&db).is_some_and(|n| n.crate_name().as_str() == bc)
                    })
            })
            .collect()
    } else {
        Vec::new()
    };
    let check_root = args.root()?;
    let check_report_path = args.out.with_extension("check.txt");
    let _ = std::fs::write(&check_report_path, "errors=0 warnings=0 us=0\n");
    let mut last_check_report = CheckReport::default();
    loop {
        if WatchedFile::watch_included(&mut files, &mut db) {
            known.touch(&db, None);
        }
        let mut client = match &listener {
            Some(listener) => requested(listener, args.watch)?,
            None => {
                std::thread::sleep(Duration::from_millis(5));
                None
            }
        };
        let mut noticed = None;
        // Keep scanning until a pass finds nothing new, so one save of several files is one build.
        // Files are read after that: a timestamp can be that of a write still under way, and
        // the rest of that write may not change it again. Whoever asks for a build has finished
        // writing, so one pass is enough then.
        let mut touched = FxHashSet::default();
        // Source files that are no longer there.
        let mut gone = FxHashSet::default();
        loop {
            let had_client = client.is_some();
            // A new file has no timestamp yet, so the pass below takes it for a changed one.
            for (file, path) in tree.added(&mut db).added {
                files.push(WatchedFile {
                    file: Watched::Source(file),
                    path,
                    mtime: SystemTime::UNIX_EPOCH,
                });
            }
            let before = touched.len() + gone.len();
            if files.len() > 256 {
                let threads = std::thread::available_parallelism().map_or(1, |it| it.get()).min(8);
                let chunk_size = files.len().div_ceil(threads).max(1);
                let results: Vec<Vec<(usize, Option<SystemTime>)>> = std::thread::scope(|scope| {
                    files
                        .chunks(chunk_size)
                        .enumerate()
                        .map(|(c_idx, chunk)| {
                            let base = c_idx * chunk_size;
                            scope.spawn(move || {
                                let mut changes = Vec::new();
                                for (off, file) in chunk.iter().enumerate() {
                                    match std::fs::metadata(&file.path).and_then(|m| m.modified()) {
                                        Ok(now) if now != file.mtime => {
                                            changes.push((base + off, Some(now)));
                                        }
                                        Err(_) => {
                                            changes.push((base + off, None));
                                        }
                                        _ => {}
                                    }
                                }
                                changes
                            })
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|h| h.join().unwrap_or_default())
                        .collect()
                });
                for (index, opt_now) in results.into_iter().flatten() {
                    match opt_now {
                        Some(now) => {
                            gone.remove(&index);
                            files[index].mtime = now;
                            touched.insert(index);
                            noticed.get_or_insert((Instant::now(), now.elapsed().unwrap_or_default()));
                        }
                        None => {
                            if matches!(files[index].file, Watched::Source(_)) && gone.insert(index) {
                                touched.remove(&index);
                                noticed.get_or_insert((Instant::now(), Duration::ZERO));
                            }
                        }
                    }
                }
            } else {
                for (index, file) in files.iter_mut().enumerate() {
                    let Ok(now) = std::fs::metadata(&file.path).and_then(|m| m.modified()) else {
                        if matches!(file.file, Watched::Source(_)) && gone.insert(index) {
                            touched.remove(&index);
                            noticed.get_or_insert((Instant::now(), Duration::ZERO));
                        }
                        continue;
                    };
                    gone.remove(&index);
                    if now != file.mtime {
                        file.mtime = now;
                        touched.insert(index);
                        noticed.get_or_insert((Instant::now(), now.elapsed().unwrap_or_default()));
                    }
                }
            }
            if client.is_none()
                && let Some(listener) = &listener
            {
                client = requested(listener, true)?;
            }
            if had_client || (client.is_none() && touched.len() + gone.len() == before) {
                break;
            }
            if client.is_none() {
                std::thread::sleep(Duration::from_millis(1));
                if let Some(listener) = &listener {
                    client = requested(listener, true)?;
                }
            }
        }
        let Some((noticed, age)) = noticed else {
            if let Some((req, client)) = client {
                if matches!(req, Request::Sync) {
                    let t0 = Instant::now();
                    let inc = bin_index
                        .as_ref()
                        .zip(args.patch.as_deref())
                        .and_then(|(idx, p)| idx.incremental_link(p));
                    if let Some(patched) = inc {
                        let elapsed = t0.elapsed();
                        eprintln!("incremental-link: {elapsed:.1?} ({patched} fns)");
                        let _ = writeln!(
                            &client,
                            "ok synced={} functions={patched} ms={}",
                            bin_index.as_ref().unwrap().bin_path.display(),
                            elapsed.as_millis()
                        );
                    } else if let Err(e) = link(&args) {
                        let _ = writeln!(&client, "error: {e}");
                    } else {
                        let _ = writeln!(
                            &client,
                            "ok synced={} ms={}",
                            args.out.display(),
                            t0.elapsed().as_millis()
                        );
                    }
                } else if matches!(req, Request::Check) {
                    let _ = writeln!(
                        &client,
                        "ok check errors={} warnings={} ms=0 report={}",
                        last_check_report.errors,
                        last_check_report.warnings,
                        check_report_path.display()
                    );
                } else {
                    let _ = writeln!(&client, "ok unchanged");
                }
            } else if !resolved_deps {
                let start = Instant::now();
                let resolved = resolve_crates(&db, &args)?;
                eprintln!("resolve: {resolved} crates in {:.1?}", start.elapsed());
                resolved_deps = true;
            } else if let Some(krate) = unprimed_crates.pop() {
                let l = listed.of(&db, krate);
                let mut baseline_dirty = false;
                hir::attach_db(&db, || {
                    let _ = hir::TraitImpls::for_crate(&db, krate.into());
                    let bin_crate = std::env::var("RUSTD_BIN_CRATE").ok();
                    if bin_crate.as_deref().is_some_and(|bc| {
                        krate.display_name(&db).is_some_and(|n| n.crate_name().as_str() == bc)
                    }) {
                        for &func in &l.functions {
                            let f = func
                                .lookup(&db)
                                .id
                                .file_id
                                .original_file_respecting_includes(&db)
                                .file_id(&db);
                            if let std::collections::hash_map::Entry::Vacant(e) =
                                initial_texts.entry(f)
                            {
                                let txt = db.file_text(f).text(&db).clone();
                                if let Some(wf) = files
                                    .iter()
                                    .find(|w| matches!(w.file, Watched::Source(wid) if wid == f))
                                    && !baseline_by_path.contains_key(&wf.path)
                                {
                                    baseline_by_path.insert(wf.path.clone(), txt.to_string());
                                    baseline_dirty = true;
                                }
                                e.insert(txt);
                                initial_adts.insert(f, snapshot_file_adts(&db, f));
                            }
                            initial_serials.entry(func).or_insert_with(|| {
                                hir_ty::mir::codegen::function_tree_serial(&db, func)
                            });
                        }
                    }
                });
                if baseline_dirty {
                    save_baseline(baseline_cache_path.as_deref(), &baseline_fp, &baseline_by_path);
                }
                if std::env::var_os("RUSTD_STATS").is_some() && unprimed_crates.is_empty() {
                    eprintln!("primed workspace caches");
                }
            }
            continue;
        };
        if !resolved_deps {
            let start = Instant::now();
            let resolved = resolve_crates(&db, &args)?;
            eprintln!("resolve: {resolved} crates in {:.1?}", start.elapsed());
            resolved_deps = true;
        }
        let settled = noticed.elapsed();
        let read = Instant::now();
        let mut change = ChangeWithProcMacros::default();
        let changed = touched.len() + gone.len();
        let mut included = Vec::new();
        let mut sources = Vec::new();
        let mut removed = Vec::new();
        for &index in &gone {
            if let Watched::Source(id) = files[index].file {
                outlines.before_edit(&db, &listed, id);
                dirty_files.remove(&id);
                change.change_file(id, None);
                sources.push(id);
                removed.push(id);
            }
        }
        let mut baseline_dirty = false;
        for index in touched {
            let WatchedFile { file, path, .. } = &files[index];
            match file {
                Watched::Source(id) => {
                    if !initial_texts.contains_key(id) {
                        let orig_arc = db.file_text(*id).text(&db).clone();
                        if !baseline_by_path.contains_key(path) {
                            baseline_by_path.insert(path.clone(), orig_arc.to_string());
                            baseline_dirty = true;
                        }
                        initial_texts.insert(*id, orig_arc);
                        initial_adts
                            .insert(*id, hir::attach_db(&db, || snapshot_file_adts(&db, *id)));
                        if args.krate.is_none() {
                            for &krate in ide_db::base_db::relevant_crates(&db, *id) {
                                let l = listed.of(&db, Crate::from(krate));
                                hir::attach_db(&db, || {
                                    for &func in &l.functions {
                                        let f = func
                                            .lookup(&db)
                                            .id
                                            .file_id
                                            .original_file_respecting_includes(&db)
                                            .file_id(&db);
                                        if f == *id {
                                            initial_serials.entry(func).or_insert_with(|| {
                                                hir_ty::mir::codegen::function_tree_serial(
                                                    &db, func,
                                                )
                                            });
                                        }
                                    }
                                });
                            }
                        }
                    }
                    let orig = initial_texts.get(id).unwrap();
                    let new_text = std::fs::read_to_string(path)?;
                    if new_text == **orig {
                        dirty_files.remove(id);
                    } else {
                        dirty_files.insert(*id);
                    }
                    outlines.before_edit(&db, &listed, *id);
                    change.change_file(*id, Some(new_text));
                    sources.push(*id);
                }
                Watched::Included(path) => included.push(path.clone()),
            }
        }
        if baseline_dirty {
            save_baseline(baseline_cache_path.as_deref(), &baseline_fp, &baseline_by_path);
        }
        let read = read.elapsed();
        eprintln!("change: {changed} files");
        let start = Instant::now();
        db.apply_change(change);
        if !removed.is_empty() {
            tree.remove(&mut db, &removed);
            let mut index = 0;
            files.retain(|_| {
                index += 1;
                !gone.contains(&(index - 1))
            });
        }
        // Asked after the change: a file can have joined a crate with it.
        if !included.is_empty() {
            known.touch(&db, None);
            listed.touch(&db, None);
        }
        for path in included {
            db.reload_included_file(&path);
        }
        // Asked of every file before any is noted: noting one forgets the functions listed.
        let edited = sources.clone();
        let sources = outlines.after_edit(&db, &listed, sources);
        for (file, bodies_only) in sources {
            if bodies_only {
                if std::env::var_os("RUSTD_STATS").is_some() {
                    eprintln!("bodies only: {file:?}");
                }
                known.touch_bodies(file);
                continue;
            }
            let crates = ide_db::base_db::relevant_crates(&db, file);
            let crates = (!crates.is_empty()).then_some(crates);
            known.touch(&db, crates);
            listed.touch(&db, crates);
        }
        let apply = start.elapsed();
        let (layout_warnings, changed_adts, changed_consts) =
            detect_adt_layout_warnings(&db, &dirty_files, &initial_adts);
        let paths: FxHashMap<vfs::FileId, PathBuf> = files
            .iter()
            .filter_map(|it| match it.file {
                Watched::Source(id) => Some((id, it.path.clone())),
                Watched::Included(_) => None,
            })
            .collect();
        let mut check_files: Vec<vfs::FileId> = edited.clone();
        for &df in &dirty_files {
            if !check_files.contains(&df) {
                check_files.push(df);
            }
        }
        let check_client = if matches!(client, Some((Request::Check, _))) {
            client.take().map(|(_, s)| s)
        } else {
            None
        };
        // The check of the edited files does not wait for the build: it reads other things.
        let (warm, check_rep) = std::thread::scope(|scope| {
            let check = {
                let (db, check_files, layout_warnings, paths, check_root, check_report_path) = (
                    db.clone(),
                    &check_files,
                    &layout_warnings,
                    &paths,
                    &check_root,
                    &check_report_path,
                );
                scope.spawn(move || {
                    let t_check = Instant::now();
                    let rep = check_files_diagnostics(
                        &db,
                        check_files,
                        layout_warnings,
                        paths,
                        check_root,
                    );
                    let check_us = t_check.elapsed().as_micros();
                    let file_body = format!(
                        "errors={} warnings={} us={}\n{}",
                        rep.errors, rep.warnings, check_us, rep.rendered
                    );
                    let tmp = check_report_path.with_extension("txt.tmp");
                    if std::fs::write(&tmp, &file_body).is_ok() {
                        let _ = std::fs::rename(&tmp, check_report_path);
                    }
                    if let Some(mut stream) = check_client {
                        let _ = writeln!(
                            stream,
                            "ok check errors={} warnings={} ms={} report={}",
                            rep.errors,
                            rep.warnings,
                            start.elapsed().as_millis(),
                            check_report_path.display()
                        );
                    }
                    rep
                })
            };
            let warm = build(
                &db,
                &table,
                &args,
                &mut known,
                &mut listed,
                &dirty_files,
                &initial_serials,
                &changed_adts,
                &changed_consts,
                false,
            );
            (warm, check.join().ok())
        });
        let warm = warm?;
        let broken = check_rep.as_ref().and_then(|r| r.first_summary.clone());
        if let Some(rep) = check_rep {
            last_check_report = rep;
        }
        let built = start.elapsed();
        eprint!("edit (apply {apply:.1?}): ");
        report(&warm);
        report_stats(&known.stats);
        let reported = start.elapsed();
        let out_of_date = hir::attach_db(&db, || known.out_of_date(&db));
        for w in &layout_warnings {
            eprintln!("warning: {w}");
        }
        let mut failed = refusal(&db, &edited, &paths, out_of_date).or(broken);
        let is_live_patch = args.patch.is_some()
            && known.stats.patched.is_some()
            && !matches!(client, Some((Request::Sync, _)));
        if failed.is_none() && !is_live_patch && !matches!(client, Some((Request::Patch, _))) {
            let t0 = Instant::now();
            let inc = if known.stats.patched.is_some() {
                bin_index
                    .as_ref()
                    .zip(args.patch.as_deref())
                    .and_then(|(idx, p)| idx.incremental_link(p))
            } else {
                None
            };
            if let Some(patched) = inc {
                eprintln!("incremental-link: {:.1?} ({patched} fns)", t0.elapsed());
            } else {
                failed = link(&args).err().map(|it| it.to_string());
            }
        }
        if let Some(why) = &failed {
            eprintln!("refused: {why}");
        }
        let total = start.elapsed();
        if std::env::var_os("RUSTD_STATS").is_some() {
            let [roots, revalidate, compile] = warm.steps;
            let phases = known.stats.phases;
            // In the order they happen, from the save on. Each is a duration of its own.
            let steps = [
                ("notice", age),
                ("settle", settled),
                ("read", read),
                ("apply", apply),
                ("roots", roots),
                // All of the checking: threads look at the kinds one after another.
                ("check", revalidate),
                ("reach", phases.compile),
                ("resolve", phases.resolve),
                ("define", phases.define),
                ("object", phases.object),
                (
                    "compile-rest",
                    compile.saturating_sub(
                        phases.compile + phases.resolve + phases.define + phases.object,
                    ),
                ),
                ("write", warm.emit),
                ("build-rest", built.saturating_sub(apply + warm.codegen + warm.emit)),
                ("report", reported - built),
                ("link", total - reported),
            ];
            let steps: Vec<String> =
                steps.iter().map(|(name, took)| format!("{name}={}", took.as_micros())).collect();
            eprintln!("timeline-us: {}", steps.join(" "));
        }
        eprintln!("edit-to-binary: {total:.1?}");
        if std::env::var_os("RUSTD_TRACE").is_some() {
            let ran: Vec<String> = ide_db::executions::take(&db)
                .iter()
                .take(400)
                .map(|(query, count)| format!("{query}={count}"))
                .collect();
            eprintln!("ran: {}", ran.join(" "));
            eprintln!("order: {}", ide_db::executions::order(&db).join(" > ").replace(' ', ""));
        }
        if std::env::var_os("RUSTD_STATS").is_some() {
            eprintln!(
                "asked: {} roots, {} instances, slowest {:.1?}; longest on a thread: roots {:.1?}, \
                 instances {:.1?}, statics {:.1?}, left {:.1?}",
                warm.looked.roots_asked,
                warm.looked.instances_asked,
                warm.looked.slowest,
                warm.looked.roots,
                warm.looked.instances,
                warm.looked.statics,
                warm.looked.left,
            );
        }
        if let (Some((_, client)), Some(why)) = (&client, &failed) {
            let _ = writeln!(&mut &*client, "error: {}", why.replace('\n', " "));
            continue;
        }
        if let Some((_, client)) = client {
            // What the caller has to load to be up to date: the functions that differ from the
            // first object when the edit could be a patch, and all of them when it could not.
            let (kind, object) = match (&args.patch, known.stats.patched) {
                (Some(patch), Some(_)) => ("patch", patch),
                _ => ("object", &args.out),
            };
            let warn_suffix = if layout_warnings.is_empty() {
                String::new()
            } else {
                format!(" warning={:?}", layout_warnings.join("; "))
            };
            let _ = writeln!(
                &client,
                "ok {kind}={} functions={} files={changed} ms={}{warn_suffix}",
                object.display(),
                known.stats.patched.unwrap_or(known.stats.compiled),
                total.as_millis()
            );
        }
    }
}

#[allow(dead_code)]
fn abs(path: &Path) -> anyhow::Result<VfsPath> {
    Ok(VfsPath::from(AbsPathBuf::assert_utf8(std::fs::canonicalize(path)?)))
}

fn report_stats(stats: &hir_ty::mir::codegen::BuildStats) {
    if stats.bodies == 0 {
        return;
    }
    eprintln!(
        "stats: {} of {} functions compiled, {} reused",
        stats.compiled, stats.bodies, stats.reused
    );
    if let Some(patched) = stats.patched {
        eprintln!("patch: {patched} functions");
    }
    if let Some(why) = stats.declined {
        eprintln!("no patch: {why}");
    }
    let phases = stats.phases;
    eprintln!(
        "phases: compile {:.1?}, resolve {:.1?}, define {:.1?}, fallback {:.1?}, object {:.1?}, {} rounds",
        phases.compile, phases.resolve, phases.define, phases.fallback, phases.object, stats.rounds
    );
}
