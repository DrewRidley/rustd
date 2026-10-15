//! Spike: keep a workspace loaded in rust-analyzer's database and compile
//! functions to native code that links against rustc-built artifacts.

mod cli;
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
use hir::{AsAssocItem, AssocItemContainer, Crate, Module, ModuleDef, db::HirDatabase};
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
        let AssocItemContainer::Impl(imp) = assoc.container(self.db) else { return None };
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
        self.table.get(&self.path(func)?).map(str::to_owned)
    }

    fn stable_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        let krate = Crate::from(krate);
        let name = krate.display_name(self.db)?;
        self.table.stable_crate_id(name.crate_name().as_str(), &self.crate_root(krate)?)
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
    let mut command = Command::new("cargo");
    command.args(["tree", "--edges", "normal", "--prefix", "none", "--format", "{p}|{f}"]);
    command.arg("--manifest-path").arg(manifest);
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
    let mut built = FxHashMap::default();
    for line in String::from_utf8(output.stdout)?.lines() {
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
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    std::thread::scope(|scope| {
        // The bodies of one file are next to each other, and the edited file's are the slow
        // ones: each thread takes every nth, so that they are spread over all of them.
        for part in 0..threads.min(bodies.len()) {
            let (db, bodies) = (db.clone(), &bodies);
            scope.spawn(move || {
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
    static PROBING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn build(
    db: &RootDatabase,
    table: &SymbolTable,
    args: &Args,
    known: &mut Rejections,
    listed: &mut Listed,
    dirty_files: &FxHashSet<vfs::FileId>,
    initial_serials: &FxHashMap<FunctionId, u64>,
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
        for &file in dirty_files {
            for &krate in ide_db::base_db::relevant_crates(db, file) {
                if seen_crates.insert(krate) {
                    let c = Crate::from(krate);
                    local.push(c);
                    let l = listed.of(db, c);
                    hir::attach_db(db, || {
                        for &func in &l.functions {
                            let f = func
                                .lookup(db)
                                .id
                                .file_id
                                .original_file_respecting_includes(db)
                                .file_id(db);
                            if dirty_files.contains(&f)
                                && initial_serials.get(&func).is_none_or(|&orig_s| {
                                    hir_ty::mir::codegen::root_serial(db, func) != orig_s
                                })
                            {
                                e.push(func);
                            } else if resolver.path(func).is_none() {
                                o.push(func);
                            }
                        }
                    });
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

/// Something written in one of the edited files that cargo turns the crate down for. Nothing
/// may reach it, so the build never looks at it.
fn broken_in(db: &RootDatabase, edited: &[vfs::FileId]) -> Option<String> {
    use hir::AnyDiagnostic as Diagnostic;
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    let broken = std::thread::scope(|scope| {
        let checks: Vec<_> = (0..threads.min(edited.len()))
            .map(|part| {
                let db = db.clone();
                scope.spawn(move || {
                    hir::attach_db(&db, || {
                        let sema = hir::Semantics::new(&db);
                        let modules = edited
                            .iter()
                            .skip(part)
                            .step_by(threads)
                            .flat_map(|&file| sema.file_to_module_defs(file));
                        for module in modules {
                            let mut found = Vec::new();
                            module.diagnostics(&db, &mut found, false);
                            // Only what rust-analyzer is as sure of as rustc is: a wrong
                            // refusal costs a cargo build.
                            let why = found.iter().find_map(|it| match it {
                                Diagnostic::TypeMismatch(_) => Some("mismatched types"),
                                Diagnostic::UnresolvedIdent(_) => Some("unresolved name"),
                                Diagnostic::MismatchedArgCount(_)
                                | Diagnostic::MismatchedTupleStructPatArgCount(_) => {
                                    Some("wrong number of arguments")
                                }
                                Diagnostic::MissingFields(_) => Some("missing fields"),
                                Diagnostic::NoSuchField(_) | Diagnostic::UnresolvedField(_) => {
                                    Some("no such field")
                                }
                                Diagnostic::UnresolvedMethodCall(_)
                                | Diagnostic::UnresolvedAssocItem(_) => Some("no such method"),
                                Diagnostic::MissingMatchArms(_)
                                | Diagnostic::NonExhaustiveLet(_) => Some("patterns not covered"),
                                Diagnostic::TraitImplMissingAssocItems(_)
                                | Diagnostic::TraitImplRedundantAssocItems(_) => {
                                    Some("an impl does not match its trait")
                                }
                                Diagnostic::UnresolvedModule(_) => Some("a module without a file"),
                                Diagnostic::UnresolvedImport(_) => Some("unresolved import"),
                                Diagnostic::UnresolvedMacroCall(_) => Some("unresolved macro"),
                                _ => None,
                            });
                            if let Some(why) = why {
                                let name = module.name(&db).map_or_else(
                                    || "the crate root".to_owned(),
                                    |it| format!("`{}`", it.display(&db, syntax::Edition::CURRENT)),
                                );
                                return Some((name, why));
                            }
                        }
                        None
                    })
                })
            })
            .collect();
        checks.into_iter().find_map(|it| it.join().ok().flatten())
    });
    let (module, why) = broken?;
    Some(format!("{module} has errors, `cargo check` shows them: {why}"))
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
}

impl Request {
    fn parse(line: &str) -> Option<Request> {
        match line {
            "sync" => Some(Request::Sync),
            "patch" => Some(Request::Patch),
            _ => None,
        }
    }

    fn line(self) -> &'static str {
        match self {
            Request::Sync => "sync\n",
            Request::Patch => "patch\n",
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
    let rt_src = include_str!("hotpatch_rt.rs");
    let rt_rs = base_o.with_extension("rt.rs");
    let rt_o = base_o.with_extension("rt.o");
    std::fs::write(&rt_rs, rt_src)?;
    let abs_patch = std::env::current_dir()?.join(patch_o);
    let status = Command::new("rustc")
        .current_dir(root)
        .args([
            "--edition=2024",
            "--crate-name=rustd_hotpatch_rt",
            "--crate-type=rlib",
            "--emit=obj",
            "-C",
            "panic=abort",
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
    let cave_o = base_o.with_extension("cave.o");
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
    let table = std::thread::spawn(move || {
        let start = Instant::now();
        let table = SymbolTable::load(&symbol_files, &root);
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

    let start = Instant::now();
    let resolved = resolve_crates(&db, &args)?;
    eprintln!("resolve: {resolved} crates in {:.1?}", start.elapsed());

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
    let mut dirty_files: FxHashSet<vfs::FileId> = FxHashSet::default();
    let table = table.join().map_err(|_| anyhow::anyhow!("loading the symbols panicked"))??;
    let cold = build(
        &db,
        &table,
        &args,
        &mut known,
        &mut listed,
        &dirty_files,
        &initial_serials,
        true,
    )?;
    eprint!("cold: ");
    report(&cold);
    report_stats(&known.stats);
    link(&args)?;
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
            let l = UnixListener::bind(path)?;
            if args.watch {
                l.set_nonblocking(true)?;
            }
            Some(l)
        }
        None => None,
    };
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
            // A new file has no timestamp yet, so the pass below takes it for a changed one.
            for (file, path) in tree.added(&mut db).added {
                files.push(WatchedFile {
                    file: Watched::Source(file),
                    path,
                    mtime: SystemTime::UNIX_EPOCH,
                });
            }
            let before = touched.len() + gone.len();
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
            if client.is_none()
                && let Some(listener) = &listener
            {
                client = requested(listener, true)?;
            }
            if touched.len() + gone.len() == before || client.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let Some((noticed, age)) = noticed else {
            if let Some((_, client)) = client {
                let _ = writeln!(&client, "ok unchanged");
            }
            continue;
        };
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
        for index in touched {
            let WatchedFile { file, path, .. } = &files[index];
            match file {
                Watched::Source(id) => {
                    if !initial_texts.contains_key(id) {
                        initial_texts.insert(*id, db.file_text(*id).text(&db).clone());
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
                                                hir_ty::mir::codegen::root_serial(&db, func)
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
        // The check of the edited files does not wait for the build: it reads other things.
        let (warm, broken) = std::thread::scope(|scope| {
            let check = {
                let (db, edited) = (db.clone(), &edited);
                scope.spawn(move || broken_in(&db, edited))
            };
            let warm = build(
                &db,
                &table,
                &args,
                &mut known,
                &mut listed,
                &dirty_files,
                &initial_serials,
                false,
            );
            (warm, check.join().ok().flatten())
        });
        let warm = warm?;
        let built = start.elapsed();
        eprint!("edit (apply {apply:.1?}): ");
        report(&warm);
        report_stats(&known.stats);
        let reported = start.elapsed();
        let out_of_date = hir::attach_db(&db, || known.out_of_date(&db));
        let paths = files
            .iter()
            .filter_map(|it| match it.file {
                Watched::Source(id) => Some((id, it.path.clone())),
                Watched::Included(_) => None,
            })
            .collect();
        let mut failed = refusal(&db, &edited, &paths, out_of_date).or(broken);
        let is_live_patch = args.patch.is_some()
            && known.stats.patched.is_some()
            && !matches!(client, Some((Request::Sync, _)));
        if failed.is_none() && !is_live_patch && !matches!(client, Some((Request::Patch, _))) {
            failed = link(&args).err().map(|it| it.to_string());
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
            let _ = writeln!(
                &client,
                "ok {kind}={} functions={} files={changed} ms={}",
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
