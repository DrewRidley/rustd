//! Spike: keep a workspace loaded in rust-analyzer's database and compile
//! functions to native code that links against rustc-built artifacts.

#![allow(clippy::print_stderr, reason = "a command line program: stderr is where it speaks")]

mod cargo;
mod content;
mod deps;
mod doctest;
mod impact;
mod inputs;
mod memory;
mod outline;
mod plan;
mod process;
mod remote;
mod socket;
mod splice;
mod stub;
mod symbols;
mod tree;
mod weaken;

/// The daemon holds millions of small allocations and frees a file's worth of them on each
/// edit, which the system allocator is slow at.
#[cfg(not(feature = "heap-profile"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "heap-profile")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use ide_db::base_db::{CrateOrigin, LangCrateOrigin, SourceDatabase as _};
use std::{
    collections::BTreeSet,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, bail};
use hir::{AsAssocItem, AssocItemContainer, Crate, Module, ModuleDef, db::HirDatabase};
use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, ItemContainerId, Lookup, ModuleDefId, StaticId,
    expr_store::Body, item_scope::ItemScope,
};
use hir_ty::mir::codegen::{
    Backend, CodegenError, CodegenUnit, ContentId, ParallelWork, Rejections, Roots, SymbolResolver,
    exported_symbol,
};
use ide_db::{ChangeWithProcMacros, RootDatabase};
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice};
use project_model::{
    CargoConfig, CargoFeatures, ProjectManifest, ProjectWorkspace, ProjectWorkspaceKind,
    RustLibSource, TargetDirectoryConfig, TargetKind,
};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, SyntaxNode,
    ast::{self, HasName},
    match_ast,
};
use vfs::{AbsPathBuf, Vfs, VfsPath};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
// The same sockets, which the standard library does not have on Windows.
#[cfg(windows)]
use uds_windows::UnixStream;

use crate::{
    splice::{Baseline, Linked, Spliced},
    symbols::SymbolTable,
};

#[derive(Clone)]
struct Args {
    project: PathBuf,
    krate: String,
    /// More crates to compile from the same loaded workspace, into the same object. When `out`
    /// is a directory, each crate is a program of its own instead: `out` and `report` get a
    /// file per crate, and a crate that fails gets a `.error` file instead of ending the run.
    batch: Vec<String>,
    /// The root files of the crates the daemon builds without having been asked to: the ones
    /// the crates asked for use, whose source is no longer what rustc compiled.
    widened: Vec<String>,
    out: PathBuf,
    symbol_files: Vec<PathBuf>,
    watch: bool,
    /// A Unix socket to take build requests on, instead of building when a file changes.
    socket: Option<PathBuf>,
    link_cmd: Option<String>,
    /// Where to write the object that amends `out` after an edit, for the link to take ahead of
    /// `out`. Without it every edit writes `out` again.
    patch: Option<PathBuf>,
    /// The executable `link_cmd` writes. With it an edit that fits a patch is added to the
    /// executable where it is, and `link_cmd` only runs when that can't be done.
    splice: Option<PathBuf>,
    /// Tab-separated per-function outcome, for the harness.
    report: Option<PathBuf>,
    /// File of function paths to leave to rustc, for bisecting miscompiles.
    deny: Option<PathBuf>,
    /// Directory that gets one object per compiled function, for testing each in isolation.
    isolate: Option<PathBuf>,
    /// Build the workspace's crates as `cargo test` does, with `cfg(test)`.
    cfg_test: bool,
    /// Run build scripts and build proc macros in a directory of rustd's own under the
    /// target directory, so that nothing here waits for a cargo that is building there.
    own_target: bool,
    /// Which build of the package this is: its profile, features, packages and target. The
    /// target is the triple to emit code for, when it is not the host: the workspace is
    /// loaded as cargo builds it for that target, and the objects are of its format and
    /// architecture.
    selection: plan::Selection,
    /// Other checkouts of projects to load into the same database, for requests that name
    /// one as their `--project`. What they have in common with `project` is there once.
    also: Vec<PathBuf>,
    /// What the code has a debugger told: what `profile` asks for.
    debug: plan::Described,
    /// The root of the workspace `project` is in. Another checkout has crates of the same
    /// names, and the ones to build are the ones below this.
    checkout: PathBuf,
    /// The loaded checkout that `checkout` reads the packages from that are the same in both.
    base: Option<PathBuf>,
    /// The crates of `checkout`. One that is the same in another checkout is of both.
    members: Arc<FxHashSet<ide_db::base_db::Crate>>,
}

impl Args {
    /// What `path` is called in the checkout, when it is a file of it.
    fn inside<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let base = self.base.as_deref().and_then(|base| path.strip_prefix(base).ok());
        path.strip_prefix(&self.checkout).ok().or(base)
    }

    /// The crates that are built into the program's object, the one asked for first.
    fn crates(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.krate).chain(&self.batch).chain(&self.widened)
    }

    /// Whether the file is one the checkout is built from. A file of the checkout it shares
    /// packages with is, when a crate of this one has it.
    fn owns(&self, db: &RootDatabase, file: vfs::FileId, path: &Path) -> bool {
        path.starts_with(&self.checkout)
            || self.base.as_ref().is_some_and(|base| {
                path.starts_with(base)
                    && ide_db::base_db::relevant_crates(db, file)
                        .iter()
                        .any(|it| self.members.contains(it))
            })
    }

    /// The directory of the project, which cargo runs rustc in.
    fn root(&self) -> anyhow::Result<PathBuf> {
        let mut root = dunce::canonicalize(&self.project)?;
        if root.is_file() {
            root.pop();
        }
        Ok(root)
    }

    fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Args> {
        let mut project = None;
        let mut krate = None;
        let mut batch = Vec::new();
        let mut out = None;
        let mut symbol_files = Vec::new();
        let mut watch = false;
        let mut socket = None;
        let mut link_cmd = None;
        let mut patch = None;
        let mut splice = None;
        let mut report = None;
        let mut deny = None;
        let mut isolate = None;
        let mut cfg_test = false;
        let mut dev_deps = false;
        let mut own_target = false;
        let mut features = Vec::new();
        let mut no_default_features = false;
        let (mut packages, mut workspace) = (Vec::new(), false);
        let mut all_features = false;
        let mut profile = None;
        let mut also = Vec::new();
        let mut target = None;
        while let Some(arg) = args.next() {
            let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--crate" if krate.is_some() => batch.push(value()?),
                "--crate" => krate = Some(value()?),
                "--out" => out = Some(PathBuf::from(value()?)),
                "--symbols" => symbol_files.push(PathBuf::from(value()?)),
                "--symbols-in" => {
                    let listed = value()?;
                    let listed = std::fs::read_to_string(&listed)
                        .with_context(|| format!("reading the symbol files in {listed}"))?;
                    symbol_files.extend(listed.lines().map(PathBuf::from));
                }
                "--link-cmd" => link_cmd = Some(value()?),
                "--patch" => patch = Some(PathBuf::from(value()?)),
                "--binary" | "--splice" => splice = Some(PathBuf::from(value()?)),
                "--report" => report = Some(PathBuf::from(value()?)),
                "--deny" => deny = Some(PathBuf::from(value()?)),
                "--isolate" => isolate = Some(PathBuf::from(value()?)),
                "--watch" => watch = true,
                "--socket" => socket = Some(PathBuf::from(value()?)),
                "--cfg-test" => cfg_test = true,
                "--dev-deps" => dev_deps = true,
                "--own-target" => own_target = true,
                "--also" => also.push(PathBuf::from(value()?)),
                "--target" => target = Some(value()?),
                "--features" => features.extend(value()?.split(',').map(str::to_owned)),
                "--no-default-features" => no_default_features = true,
                "--package" | "-p" => packages.push(value()?),
                "--workspace" => workspace = true,
                "--all-features" => all_features = true,
                "--release" | "--profile" if profile.is_some() => {
                    bail!("the profile is given twice")
                }
                "--release" => profile = Some("release".to_owned()),
                "--profile" => profile = Some(value()?),
                _ if project.is_none() => project = Some(PathBuf::from(arg)),
                _ => bail!("unexpected argument {arg}"),
            }
        }
        Ok(Args {
            project: project.context("missing project path")?,
            krate: krate.context("missing --crate")?,
            batch,
            widened: Vec::new(),
            out: out.context("missing --out")?,
            symbol_files,
            watch,
            socket,
            link_cmd,
            patch,
            splice,
            report,
            deny,
            isolate,
            cfg_test,
            own_target,
            selection: plan::Selection {
                profile: profile.unwrap_or_else(|| "dev".to_owned()),
                features,
                no_default_features,
                all_features,
                packages,
                workspace,
                target,
                dev_deps: cfg_test || dev_deps,
            },
            also,
            debug: plan::Described::NOTHING,
            checkout: PathBuf::new(),
            base: None,
            members: Arc::default(),
        })
    }
}

struct Resolver<'a> {
    db: &'a RootDatabase,
    table: &'a SymbolTable,
    /// What panic locations in the workspace are relative to.
    root: PathBuf,
    /// The checkout that the packages which are the same in both are read from.
    base: Option<PathBuf>,
    rustc_commit: std::cell::OnceCell<Option<String>>,
    contents: &'a content::Contents,
    /// What the code has a debugger told.
    debug: plan::Described,
    /// The object the entries a debugger reads are kept in, when they are kept apart from
    /// the program.
    apart: Option<PathBuf>,
    listed: &'a Listed,
    /// For each crate asked about, the paths that functions written in its bodies share.
    shared_paths: std::cell::RefCell<FxHashMap<Crate, FxHashSet<String>>>,
}

impl Resolver<'_> {
    /// What `path` is called in the checkout, when it is a file of it.
    fn inside<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let base = self.base.as_deref().and_then(|base| path.strip_prefix(base).ok());
        path.strip_prefix(&self.root).ok().or(base)
    }

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
                    let default = param.as_type_param(self.db).and_then(|it| it.default(self.db));
                    // FIXME: a default that names a parameter is understood only as `= Self`.
                    let is_default = default.as_ref().is_some_and(|default| {
                        if default.as_type_param(self.db).is_some() {
                            return hir::attach_db(self.db, || arg == trait_ref.self_ty());
                        }
                        let default = self.type_path(default);
                        default.is_some() && default == self.type_path(&arg)
                    });
                    if idx + 1 == params.len() && is_default {
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

    /// Whether the function, or the `impl` it is in, takes types or constants.
    fn is_generic(&self, func: FunctionId) -> bool {
        let generic = |def: hir::GenericDef| !def.type_or_const_params(self.db).is_empty();
        if generic(hir::Function::from(func).into()) {
            return true;
        }
        let ItemContainerId::ImplId(imp) = func.lookup(self.db).container else { return false };
        generic(hir::Impl::from(imp).into())
    }

    /// Whether another function written in a body of the crate has this one's path. rustc
    /// tells such functions apart by a number that the path does not show, so its symbol for
    /// the path may be the other's.
    fn shares_path(&self, func: FunctionId) -> bool {
        let module = hir::Function::from(func).module(self.db);
        if !is_in_body(self.db, module) {
            return false;
        }
        let Some(path) = self.path(func) else { return false };
        let krate = module.krate(self.db);
        if let Some(shared) = self.shared_paths.borrow().get(&krate) {
            return shared.contains(&path);
        }
        let shared = self.paths_shared_in(krate);
        let shares = shared.contains(&path);
        self.shared_paths.borrow_mut().insert(krate, shared);
        shares
    }

    /// The paths that more than one of the functions written in the bodies of a crate have.
    fn paths_shared_in(&self, krate: Crate) -> FxHashSet<String> {
        let mut seen = FxHashSet::default();
        let mut shared = FxHashSet::default();
        // Asked for without listing the crate as one that is built.
        let listed = self.listed.functions.lock().unwrap().get(&krate).cloned();
        let functions = match &listed {
            Some(listed) => listed.functions.clone(),
            None => functions_of(self.db, krate),
        };
        for func in functions {
            if !is_in_body(self.db, hir::Function::from(func).module(self.db)) {
                continue;
            }
            let Some(path) = self.path(func) else { continue };
            if !seen.insert(path.clone()) {
                shared.insert(path);
            }
        }
        shared
    }

    /// rustc's `StableCrateId` of a crate it built something of. A crate of the checkout is
    /// one rustc built when a file of the link is its own: the standard library has private
    /// copies of crates (memchr, hashbrown) whose names a dependency at a path has too.
    fn rustc_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        let ours = !krate.data(self.db).origin.is_lang();
        let krate = Crate::from(krate);
        let name = krate.display_name(self.db)?;
        let root = self.crate_root(krate)?;
        if ours && !self.table.links(name.crate_name().as_str(), &root) {
            return None;
        }
        self.table.stable_crate_id(name.crate_name().as_str(), &root)
    }

    /// How the paths of symbols spell a crate. One of the checkout that rustc built nothing
    /// of is spelled with its own id, so that no symbol of another crate of its name (the
    /// standard library's memchr) is taken for one of its items.
    fn crate_path(&self, krate: Crate) -> Option<String> {
        let name = krate.display_name(self.db)?;
        match self.own_crate_id(krate.into()) {
            Some(own) => Some(format!("{}[{own:x}]", name.crate_name())),
            None => self.table.crate_path(name.crate_name().as_str(), &self.crate_root(krate)?),
        }
    }

    /// What stands for a crate of the checkout that rustc built nothing of, the same after
    /// every edit: its name and where in the checkout its source starts.
    fn own_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        if krate.data(self.db).origin.is_lang() || self.rustc_crate_id(krate).is_some() {
            return None;
        }
        let name = Crate::from(krate).display_name(self.db)?;
        let root = self.crate_root(krate.into())?;
        let root = self.inside(&root).unwrap_or(&root);
        let named = format!("rustd crate {} {}", name.crate_name(), root.display());
        Some(short_hash(&named))
    }

    /// The file the crate's source starts at.
    fn crate_root(&self, krate: Crate) -> Option<PathBuf> {
        crate_root_file(self.db, krate)
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
            let mut path = self.crate_path(module.krate(self.db))?;
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
        if ty.is_tuple() {
            let fields: Option<Vec<_>> =
                ty.tuple_fields(self.db).iter().map(|it| self.type_path(it)).collect();
            return Some(match &*fields? {
                [only] => format!("({only},)"),
                fields => format!("({})", fields.join(", ")),
            });
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
/// standard library's files: as rustup's manifest has it, or else as the built library does.
fn rustc_commit(toolchain: &str) -> Option<String> {
    let rustlib = format!("{toolchain}lib/rustlib");
    commit_in_manifest(&rustlib).or_else(|| commit_in_library(Path::new(&rustlib)))
}

fn commit_in_manifest(rustlib: &str) -> Option<String> {
    let manifest =
        std::fs::read_to_string(format!("{rustlib}/multirust-channel-manifest.toml")).ok()?;
    let (_, rust) = manifest.split_once("[pkg.rust]")?;
    let (_, commit) = rust.split_once("git_commit_hash = \"")?;
    Some(commit.split_once('"')?.0.to_owned())
}

/// The commit in the paths a built standard library below `rustlib` has its sources under.
fn commit_in_library(rustlib: &Path) -> Option<String> {
    let targets = std::fs::read_dir(rustlib).ok()?;
    let libraries = targets.filter_map(|it| std::fs::read_dir(it.ok()?.path().join("lib")).ok());
    let mut built: Vec<PathBuf> = libraries
        .flatten()
        .filter_map(|it| Some(it.ok()?.path()))
        .filter(|it| {
            let name = it.file_name().and_then(|it| it.to_str()).unwrap_or_default();
            name.starts_with("libstd-") && name.ends_with(".rlib")
        })
        .collect();
    built.sort_unstable();
    built.iter().find_map(|it| commit_of_sources(&std::fs::read(it).ok()?))
}

/// The commit of the first `/rustc/<commit>/` in what rustc built.
fn commit_of_sources(built: &[u8]) -> Option<String> {
    const UNDER: &[u8] = b"/rustc/";
    const LENGTH: usize = 40;
    memchr::memmem::find_iter(built, UNDER).find_map(|at| {
        let commit = built.get(at + UNDER.len()..at + UNDER.len() + LENGTH + 1)?;
        let (slash, commit) = commit.split_last()?;
        let named = *slash == b'/' && commit.iter().all(u8::is_ascii_hexdigit);
        named.then(|| String::from_utf8_lossy(commit).into_owned())
    })
}

/// What tells the toolchain whose library was loaded from any other: the name of the built
/// library the program links, its commit, and the name of its compiler, each of which has
/// a hash of the toolchain in it. `None` when it has none of them.
fn toolchain_identity(vfs: &vfs::Vfs, symbol_files: &[PathBuf]) -> Option<String> {
    let mut identity: Vec<String> = symbol_files
        .iter()
        .filter_map(|it| it.file_name()?.to_str())
        .filter(|it| it.starts_with("libstd-"))
        .map(str::to_owned)
        .collect();
    let toolchain = vfs.iter().find_map(|(_, path)| {
        let path = path.as_path()?.as_str().replace('\\', "/");
        let (toolchain, _) = path.split_once("lib/rustlib/src/rust/")?;
        Some(toolchain.to_owned())
    });
    if let Some(toolchain) = &toolchain {
        identity.extend(rustc_commit(toolchain));
        for dir in ["lib", "bin"] {
            let Ok(entries) = std::fs::read_dir(format!("{toolchain}{dir}")) else { continue };
            let names = entries.filter_map(|it| it.ok()?.file_name().into_string().ok());
            identity.extend(names.filter(|it| it.contains("rustc_driver-")));
        }
    }
    identity.sort_unstable();
    (!identity.is_empty()).then(|| identity.join(" "))
}

impl SymbolResolver for Resolver<'_> {
    fn in_parallel(&self, count: usize, work: &ParallelWork<'_>) {
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get()).min(count);
        if threads < 2 {
            return;
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let db = self.db.clone();
                let (next, table, root) = (&next, self.table, self.root.clone());
                let base = self.base.clone();
                let (rustc_commit, contents) = (self.rustc_commit.clone(), self.contents);
                let (debug, apart) = (self.debug, self.apart.clone());
                let listed = self.listed;
                scope.spawn(move || {
                    let resolver = Resolver {
                        db: &db,
                        table,
                        root,
                        base,
                        rustc_commit,
                        contents,
                        debug,
                        apart,
                        listed,
                        shared_paths: Default::default(),
                    };
                    hir::attach_db(&db, || {
                        loop {
                            let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if at >= count {
                                break;
                            }
                            work(&db, &resolver, at);
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
        // Whichever way this system writes a path; what follows the commit stays as it is.
        const SOURCES: &str = "lib/rustlib/src/rust/";
        let slashed = path.replace('\\', "/");
        if let Some((toolchain, _)) = slashed.split_once(SOURCES)
            && let Some(commit) = self.rustc_commit.get_or_init(|| rustc_commit(toolchain))
        {
            let file = &path[toolchain.len() + SOURCES.len()..];
            return Some(format!("/rustc/{commit}/{file}"));
        }
        let relative = self.inside(Path::new(&path));
        Some(relative.map_or(path.clone(), |it| it.to_string_lossy().into_owned()))
    }

    fn debug_directory(&self) -> Option<String> {
        (self.debug.level != plan::Level::Nothing).then(|| self.root.to_string_lossy().into_owned())
    }

    fn describes_variables(&self) -> bool {
        self.debug.level == plan::Level::Full
    }

    fn debug_apart(&self) -> Option<String> {
        self.apart.as_ref().map(|it| it.to_string_lossy().into_owned())
    }

    fn function(&self, func: FunctionId) -> Option<String> {
        if let Some(symbol) = exported_symbol(self.db, func) {
            return Some(symbol);
        }
        // Not a function until it or the `impl<T>` it is in is given its types: there is
        // nothing to name, the more so in a crate of such alone, which rustc builds nothing
        // of. One of a name that another in the same function has is not to be taken for it.
        if self.is_generic(func) {
            return None;
        }
        // The symbol of a path is the one rustc gave a function it built under it, which is
        // not known to be this one when a body has two of the path.
        if self.shares_path(func) {
            return None;
        }
        let path = self.path(func)?;
        // A test is the second of its name where it is written: rustc puts the constant
        // that describes it to the harness before it, under the same name.
        let function = hir::Function::from(func);
        let symbol = if function.is_test(self.db) || function.is_bench(self.db) {
            self.table.get_at(&path, 1)
        } else {
            self.table.get(&path)
        };
        if let Some(symbol) = symbol {
            return Some(symbol.to_owned());
        }
        // rustc has built nothing of the crate: the function is ours to name, unless it is
        // declared in an `extern` block, where what it links to names it.
        if matches!(func.lookup(self.db).container, ItemContainerId::ExternBlockId(_)) {
            return None;
        }
        let krate = hir_def::HasModule::krate(&func, self.db);
        let id = self.own_crate_id(krate)?;
        match hir_ty::mir::codegen::block_place(self.db, self, func) {
            Some(place) => Some(format!("rustd.{id:x}.{path}.{:x}", short_hash(&place))),
            None => Some(format!("rustd.{id:x}.{path}")),
        }
    }

    fn private(&self, func: FunctionId) -> bool {
        !self.is_generic(func) && self.path(func).is_some_and(|path| self.table.private(&path))
    }

    fn defines(&self, symbol: &str) -> bool {
        self.table.defines(symbol)
    }

    fn makers(&self, function: &str) -> Vec<u64> {
        self.table.makers(function).collect()
    }

    fn copies(&self, symbol: &str) -> Vec<(String, u64)> {
        self.table.copies(symbol).map(|(copy, of)| (copy.to_owned(), of)).collect()
    }

    fn rustc_built(&self, krate: ide_db::base_db::Crate) -> bool {
        self.own_crate_id(krate).is_none()
    }

    fn stable_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        self.rustc_crate_id(krate).or_else(|| self.own_crate_id(krate))
    }

    fn content_id(&self, krate: ide_db::base_db::Crate) -> Option<ContentId> {
        let places = (&*self.root, self.base.as_deref());
        if !krate.data(self.db).origin.is_local() {
            let root = self.crate_root(krate.into())?;
            return Some(self.contents.library(
                self.db,
                krate,
                &root,
                &|krate| Some((self.stable_crate_id(krate), self.crate_root(krate.into())?)),
                &|krate| process_reads(self.db, krate),
            ));
        }
        self.contents.id(
            self.db,
            places,
            krate,
            &|krate| self.stable_crate_id(krate),
            // Every body of the crate has been looked through once it is listed, so that
            // every variable it asks for is known.
            &|krate| {
                self.listed.of(self.db, krate.into());
                process_reads(self.db, krate)
            },
        )
    }

    fn cargo_home(&self) -> Option<String> {
        self.contents.cargo_home(self.db)
    }

    fn names_place(&self, bytes: &[u8]) -> bool {
        self.contents.names_place((&*self.root, self.base.as_deref()), bytes)
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

/// Whether what the module declares is written in a body.
fn is_in_body(db: &RootDatabase, module: Module) -> bool {
    module.nearest_non_block_module(db) != module
}

/// The first eight bytes of the SHA-256 of a text.
fn short_hash(text: &str) -> u64 {
    let hash = <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes());
    u64::from_le_bytes(hash[..8].try_into().expect("a hash is 32 bytes"))
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

/// The crates called `krate` that a build of the checkout at `checkout` compiles: a package's
/// library and its binary may share the name, and both are being built. A `krate` that is the
/// path of a `.rs` file is the one target with that root file, which tells a test from an
/// example of the same name.
fn crates_named(db: &RootDatabase, krate: &str, args: &Args) -> anyhow::Result<Vec<Crate>> {
    let mut found = crates_called(db, krate, args);
    // A dependency may be another version of the crate being built, and another checkout
    // has a crate of its name too.
    let of_checkout = |krate: &Crate| args.members.contains(&(*krate).into());
    if found.iter().any(of_checkout) {
        found.retain(of_checkout);
    }
    anyhow::ensure!(!found.is_empty(), "crate {krate} not found in workspace");
    Ok(found)
}

/// Why what rustc built does not say which copy of a function a call reaches any more, in a
/// program whose crates differ in `-C overflow-checks`: rustc decides by the source which
/// crate compiles a function, and a source was written after it did.
fn stale_copies(db: &RootDatabase, table: &SymbolTable, args: &Args) -> Option<String> {
    let written = table.written_since_built()?;
    let built = args.crates().filter_map(|it| crates_named(db, it, args).ok()).flatten();
    let built: Vec<Crate> = built.collect();
    checks_differ(db, &built).then(|| {
        format!(
            "the crates of the program differ in `overflow-checks` and {} was written after \
             rustc built it: which crate's copy of a function a call reaches is read from \
             what rustc built, so cargo has to build first",
            args.inside(written).unwrap_or(written).display()
        )
    })
}

/// Whether the crates cargo builds of the program of `built` do not all have
/// `-C overflow-checks` alike.
fn checks_differ(db: &RootDatabase, built: &[Crate]) -> bool {
    let flag = cfg::CfgAtom::Flag(intern::sym::overflow_checks);
    let program: FxHashSet<ide_db::base_db::Crate> = built
        .iter()
        .flat_map(|&it| ide_db::base_db::Crate::from(it).transitive_deps(db))
        .filter(|it| !it.data(db).origin.is_lang())
        .collect();
    let mut checks = program.iter().map(|it| it.cfg_options(db).check_atom(&flag));
    checks.next().is_some_and(|first| checks.any(|it| it != first))
}

/// Fails when rustc linked none of the libraries rustd builds a program with `lto` from:
/// rustc then compiled the whole program from bitcode as it linked it, and the functions
/// rustd leaves to rustc are not there to be linked against by name.
fn linked_from_libraries(db: &RootDatabase, args: &Args) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.link_cmd.as_deref().is_none_or(links_core),
        "the link command has no library of `core`, as rustc's has not with `lto`: rustd \
         links against a build with `lto = \"off\"`"
    );
    if args.symbol_files.is_empty() {
        return Ok(());
    }
    let mut built = Vec::new();
    for named in args.crates() {
        built.extend(crates_named(db, named, args)?);
    }
    let used: FxHashSet<Crate> =
        built.iter().flat_map(|it| it.dependencies(db)).map(|it| it.krate).collect();
    // A proc macro is a library of the compiler's, not of the program.
    let is_proc_macro = |krate: &Crate| ide_db::base_db::Crate::from(*krate).data(db).is_proc_macro;
    let libraries: Vec<String> = built
        .iter()
        .filter(|it| used.contains(it) && !is_proc_macro(it))
        .filter_map(|it| Some(format!("lib{}", it.display_name(db)?.crate_name().as_str())))
        .collect();
    let linked = |library: &String| {
        let mut files = args.symbol_files.iter().filter_map(|it| it.file_name()?.to_str());
        files.any(|it| it.starts_with(library.as_str()))
    };
    anyhow::ensure!(
        libraries.is_empty() || libraries.iter().any(linked),
        "rustc linked the program without a library of `{}`, as it does with `lto`: rustd \
         links against a build with `lto = \"off\"`",
        libraries[0].trim_start_matches("lib")
    );
    Ok(())
}

/// Whether a link command names `core` as a library, by itself or in the shared library of
/// std. One that reads its files from elsewhere is taken to.
fn links_core(command: &str) -> bool {
    let mut files = command.split_whitespace().map(|it| it.trim_matches(['"', '\'']));
    files.any(|it| {
        let name = it.rsplit(['/', '\\']).next().unwrap_or(it);
        it.starts_with('@') || name.starts_with("libcore-") || name.starts_with("libstd-")
    })
}

/// Every crate called `krate`, of the checkout or not.
fn crates_called(db: &RootDatabase, krate: &str, args: &Args) -> Vec<Crate> {
    let is_file = krate.ends_with(".rs");
    Crate::all(db)
        .into_iter()
        .filter(|it| match is_file {
            // Named from the checkout, or in full.
            true => crate_root_file(db, *it).is_some_and(|root| {
                root == Path::new(krate) || args.inside(&root) == Some(Path::new(krate))
            }),
            false => it.display_name(db).is_some_and(|name| name.crate_name().as_str() == krate),
        })
        .collect()
}

/// The crates called `krate` to build: the ones rustc built into this program, when it built
/// any, which may be a crate of the checkout and a dependency of the same name both. Otherwise
/// what `crates_named` finds.
fn crates_to_build(
    db: &RootDatabase,
    table: &SymbolTable,
    krate: &str,
    args: &Args,
) -> anyhow::Result<Vec<Crate>> {
    let linked = |krate: &Crate| {
        let name = krate.display_name(db);
        let root = crate_root_file(db, *krate);
        name.zip(root).is_some_and(|(name, root)| table.links(name.crate_name().as_str(), &root))
    };
    let mut called = crates_called(db, krate, args);
    called.retain(linked);
    if called.is_empty() { crates_named(db, krate, args) } else { Ok(called) }
}

fn crate_root_file(db: &RootDatabase, krate: Crate) -> Option<PathBuf> {
    let file = krate.root_file(db);
    let source_root = db.file_source_root(file).source_root_id(db);
    let source_root = db.source_root(source_root).source_root(db);
    Some(PathBuf::from(source_root.path_for_file(&file)?.as_path()?.to_string()))
}

/// Resolves the names and collects the impls of the crates being built and of everything they
/// depend on, a crate per thread. Every build needs them, and one thread would do them in turn.
/// `load_workspace_at`, with each package's features those of a build of the package at
/// `project` alone, as the artifacts we link against were built.
fn load(
    projects: &[&Path],
    cargo_config: &CargoConfig,
    load_config: LoadCargoConfig,
    selection: plan::Selection,
) -> anyhow::Result<(RootDatabase, vfs::Vfs, Checkouts)> {
    let mut checkouts = Checkouts {
        compared: Default::default(),
        roots: Vec::new(),
        splits: Vec::new(),
        loaded: load_cargo::Loaded::default(),
        cargo_config: cargo_config.clone(),
        load_config,
        selection,
        inputs: inputs::Inputs::default(),
        debug: Vec::new(),
    };
    let mut db = RootDatabase::new(None);
    let mut vfs = vfs::Vfs::default();
    checkouts.add(projects, &mut db, &mut vfs)?;
    Ok((db, vfs, checkouts))
}

/// A file of a checkout that was compared with the one it is in a loaded checkout: the size
/// and the time of the last write of both, and whether the two were alike. They are not read
/// again until one is written.
type Compared = std::sync::Mutex<FxHashMap<PathBuf, ((u64, SystemTime), (u64, SystemTime), bool)>>;

/// The checkouts the daemon has loaded.
struct Checkouts {
    compared: Compared,
    /// The project a build names, and the root of the workspace it is in.
    roots: Vec<(PathBuf, PathBuf)>,
    /// For each of `roots`, what it shares with another when it is not loaded in full.
    splits: Vec<Option<Split>>,
    loaded: load_cargo::Loaded,
    cargo_config: CargoConfig,
    load_config: LoadCargoConfig,
    /// What the checkouts loaded in full were loaded from besides their source.
    inputs: inputs::Inputs,
    /// Which build of each project is loaded.
    selection: plan::Selection,
    /// For each of `roots`, what its plan says of the program as a whole.
    debug: Vec<plan::Whole>,
}

impl Checkouts {
    fn root_of(&self, project: &Path) -> anyhow::Result<PathBuf> {
        let project = dunce::canonicalize(project)?;
        let root = self.roots.iter().find(|(it, _)| *it == project).map(|(_, root)| root.clone());
        root.with_context(|| format!("{} is not a project the daemon loaded", project.display()))
    }

    /// The checkout of `project`: its root, what it shares and its crates.
    fn parts(&self, project: &Path) -> anyhow::Result<Parts> {
        let root = self.root_of(project)?;
        let at = self.roots.iter().position(|(_, it)| *it == root).expect("it has a root");
        let base = self.splits[at].as_ref().map(|it| it.base.clone());
        let members = Arc::new(self.loaded.crates(at).iter().copied().collect());
        // An override for measuring one against the other.
        let level = match std::env::var("RUSTD_DEBUGINFO").ok().as_deref() {
            Some("0") => plan::Level::Nothing,
            Some("lines") => plan::Level::Lines,
            Some(_) => plan::Level::Full,
            None => self.debug[at].debug.level,
        };
        let debug = plan::Described { level, ..self.debug[at].debug };
        Ok(Parts { root, base, members, debug })
    }

    /// Whether the program of the checkout `project` is of is one rustc links from bitcode.
    fn lto(&self, project: &Path) -> anyhow::Result<bool> {
        let root = self.root_of(project)?;
        Ok(self.roots.iter().zip(&self.debug).any(|((_, it), whole)| *it == root && whole.lto))
    }

    /// The loaded checkout that `project` is a checkout of the same repository as, and the
    /// root of its own. `None` when the two are not built alike.
    fn alike(&self, project: &Path) -> Option<(usize, PathBuf)> {
        let project = dunce::canonicalize(project).ok()?;
        (0..self.roots.len()).find_map(|at| {
            let (loaded, base) = &self.roots[at];
            if self.splits[at].is_some() {
                return None;
            }
            let inside = loaded.strip_prefix(base).ok()?;
            let mut over = project.clone();
            for part in inside.components().rev() {
                if over.file_name() != Some(part.as_os_str()) {
                    return None;
                }
                over.pop();
            }
            self.builds_alike(base, &over).then_some((at, over))
        })
    }

    /// The directories of the member packages of the checkout at `at`, and whether each is
    /// of a procedural macro.
    fn packages(&self, at: usize) -> Vec<(PathBuf, bool)> {
        let ProjectWorkspaceKind::Cargo { cargo, .. } = &self.loaded.workspaces()[at].kind else {
            return Vec::new();
        };
        cargo
            .packages()
            .filter(|&it| cargo[it].is_member)
            .map(|it| {
                let package = &cargo[it];
                let dir = AsRef::<Path>::as_ref(package.manifest.parent()).to_owned();
                (dir, package.targets.iter().any(|&it| cargo[it].kind.is_proc_macro()))
            })
            .collect()
    }

    /// The packages of the checkout at `over` whose files are not those of the loaded one at
    /// `base`.
    fn differing(&self, base: usize, over: &Path) -> BTreeSet<PathBuf> {
        let packages = self.packages(base);
        let base = &self.roots[base].1;
        let nested: FxHashSet<&Path> = packages.iter().map(|it| &*it.0).collect();
        // A procedural macro is rustc's to build: one that is not the same is another build.
        packages
            .iter()
            .filter(|(dir, proc_macro)| {
                !proc_macro && !package_alike(dir, base, over, &nested, &self.compared)
            })
            .map(|it| &it.0)
            .filter_map(|dir| Some(dir.strip_prefix(base).ok()?.to_owned()))
            .collect()
    }

    /// The workspace of the checkout at `over`: that of the loaded one at `base`, with the
    /// packages of `own` where `over` has them.
    fn split(&self, base: usize, over: &Path, own: &BTreeSet<PathBuf>) -> ProjectWorkspace {
        let mut workspace = self.loaded.workspaces()[base].clone();
        let root = AbsPathBuf::assert_utf8(self.roots[base].1.clone());
        let over = AbsPathBuf::assert_utf8(over.to_owned());
        if let ProjectWorkspaceKind::Cargo { cargo, .. } = &mut workspace.kind {
            *cargo = cargo.placed(&root, &over, &|dir| {
                dir.strip_prefix(&root).is_some_and(|it| own.contains(AsRef::<Path>::as_ref(&it)))
            });
        }
        workspace
    }

    /// The checkouts that share other packages with a loaded one than the ones that are the
    /// same in both now: each with the one it shares with and the packages of its own.
    fn differences(&self) -> anyhow::Result<Vec<(usize, usize, BTreeSet<PathBuf>)>> {
        let mut differences = Vec::new();
        for (at, split) in self.splits.iter().enumerate() {
            let Some(split) = split else { continue };
            let base = self.roots.iter().position(|(_, it)| *it == split.base);
            let base = base.context("a checkout shares with a loaded one")?;
            let own = self.differing(base, &self.roots[at].1);
            if own != split.own {
                differences.push((at, base, own));
            }
        }
        Ok(differences)
    }

    /// Gives the checkouts that share packages with a loaded one the packages of their own
    /// that [`Checkouts::differences`] found.
    fn share(
        &mut self,
        db: &mut RootDatabase,
        vfs: &mut vfs::Vfs,
        differences: Vec<(usize, usize, BTreeSet<PathBuf>)>,
    ) -> anyhow::Result<()> {
        for (at, base, own) in differences {
            let over = self.roots[at].1.clone();
            eprintln!("split {}: its own are {own:?}", over.display());
            self.loaded.replace(at, self.split(base, &over, &own));
            self.splits[at] = Some(Split { base: self.roots[base].1.clone(), own });
        }
        self.loaded.add(Vec::new(), &self.cargo_config.extra_env, &self.load_config, vfs, db)
    }

    /// How the daemon builds `project`: as itself when its checkout is loaded, and as its
    /// place in a loaded checkout with the files of its own when `overlay` allows that and
    /// the two lock the same dependencies. `None` when its checkout has to be loaded.
    fn view_of(
        &self,
        project: &Path,
        overlay: Overlay,
        quick: &dyn Fn(&Path, &Path) -> bool,
    ) -> Option<View> {
        let project = dunce::canonicalize(project).ok()?;
        if let Some((_, root)) = self.roots.iter().find(|(it, _)| *it == project) {
            return Some(View { base: root.clone(), over: root.clone(), project });
        }
        if overlay == Overlay::Never {
            return None;
        }
        // One that shares packages with another has those where the other is.
        let whole = self.roots.iter().zip(&self.splits).filter(|it| it.1.is_none()).map(|it| it.0);
        whole.into_iter().find_map(|(loaded, base)| {
            let inside = loaded.strip_prefix(base).ok()?;
            let mut over = project.clone();
            for part in inside.components().rev() {
                if over.file_name() != Some(part.as_os_str()) {
                    return None;
                }
                over.pop();
            }
            let alike = (overlay == Overlay::Always || quick(base, &over))
                && self.builds_alike(base, &over);
            alike.then(|| View { base: base.clone(), over, project: loaded.clone() })
        })
    }

    /// The checkout that the one at `root` has packages of, when it has.
    fn shares_with(&self, root: &Path) -> Option<&Path> {
        let at = self.roots.iter().position(|it| it.1 == root)?;
        self.splits[at].as_ref().map(|it| &*it.base)
    }

    /// Whether what cargo and rustc did for the checkout at `base` is what they would do for
    /// the one at `over`: the same crates with the same features, and the same build scripts
    /// and proc macros, which the daemon has the output of and does not run.
    // FIXME: a build script or a proc macro that reads other files of its checkout is not
    // noticed, and neither is a crate outside the checkout that the manifests name by path.
    fn builds_alike(&self, base: &Path, over: &Path) -> bool {
        let alike = |path: &Path| {
            let Ok(inside) = path.strip_prefix(base) else { return true };
            std::fs::read(path).ok() == std::fs::read(over.join(inside)).ok()
        };
        let top = ["Cargo.lock", "Cargo.toml", ".cargo/config.toml", "rust-toolchain.toml"];
        if !top.iter().all(|it| alike(&base.join(it))) {
            return false;
        }
        let cargo = self.loaded.workspaces().iter().find_map(|it| match &it.kind {
            ProjectWorkspaceKind::Cargo { cargo, .. }
                if AsRef::<Path>::as_ref(cargo.workspace_root()) == base =>
            {
                Some(cargo)
            }
            _ => None,
        });
        let Some(cargo) = cargo else { return false };
        cargo.packages().map(|it| &cargo[it]).filter(|it| it.is_member).all(|package| {
            let manifest: &Path = package.manifest.as_ref();
            alike(manifest)
                && package.targets.iter().map(|&it| &cargo[it]).all(|target| {
                    let root: &Path = target.root.as_ref();
                    match target.kind {
                        TargetKind::BuildScript => alike(root),
                        TargetKind::Lib { is_proc_macro: true } => {
                            root.parent().is_some_and(|it| tree_alike(it, base, over))
                        }
                        _ => true,
                    }
                })
        })
    }

    /// Loads the checkouts of `projects` into `db`, next to those there are.
    fn add(
        &mut self,
        projects: &[&Path],
        db: &mut RootDatabase,
        vfs: &mut vfs::Vfs,
    ) -> anyhow::Result<()> {
        let mut workspaces = Vec::new();
        let mut roots = Vec::new();
        let (mut splits, mut debug) = (Vec::new(), Vec::new());
        // A checkout like a loaded one shares the packages they have alike, unless told not to.
        let share = std::env::var_os("RUSTD_SPLIT").is_none_or(|it| it != "0");
        for project in projects {
            if share && let Some((base, over)) = self.alike(project) {
                let own = self.differing(base, &over);
                eprintln!("split {}: its own are {own:?}", over.display());
                workspaces.push(self.split(base, &over, &own));
                splits.push(Some(Split { base: self.roots[base].1.clone(), own }));
                debug.push(self.debug[base]);
                roots.push((dunce::canonicalize(project)?, over));
                continue;
            }
            splits.push(None);
            let (workspace, told) =
                workspace_at(project, &self.cargo_config, &self.load_config, &self.selection)?;
            debug.push(told);
            // The checkouts after the first run their build scripts where the first did. A
            // dependency is then told the same `OUT_DIR` in each, which it has to be to be one
            // crate for all.
            if let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind
                && !matches!(
                    self.cargo_config.target_dir_config,
                    TargetDirectoryConfig::Directory(_)
                )
            {
                let target: &paths::Utf8Path = cargo.target_directory().as_ref();
                let scripts = self.cargo_config.target_dir_config.target_dir(Some(target));
                self.cargo_config.target_dir_config = TargetDirectoryConfig::Directory(
                    scripts.map_or_else(|| target.to_owned(), |it| it.into_owned()),
                );
            }
            let root = PathBuf::from(workspace.workspace_root().to_string());
            self.inputs.watch(&root, &workspace, &self.cargo_config);
            roots.push((dunce::canonicalize(project)?, root));
            workspaces.push(workspace);
        }
        self.loaded.add(workspaces, &self.cargo_config.extra_env, &self.load_config, vfs, db)?;
        self.roots.extend(roots);
        self.splits.extend(splits);
        self.debug.extend(debug);
        Ok(())
    }

    /// The checkouts that are no longer what they were loaded from, for a build of
    /// `project` by a process with the variables `env`. A checkout loaded in full is one
    /// when its manifests, cargo's configuration or what its build scripts say changed, and
    /// when the build is of it and the process has other flags for rustc. One that shares
    /// packages with another is one when the build is of it and the two no longer build alike.
    fn stale(&mut self, project: &Path, env: &ide_db::base_db::Env) -> anyhow::Result<Vec<usize>> {
        let project = dunce::canonicalize(project).ok();
        let asked = self.roots.iter().position(|(it, _)| Some(it) == project.as_ref());
        // The checkout whose variables are those of the build: any, when it is not loaded.
        let built = match asked {
            Some(at) => Some(self.splits[at].as_ref().map_or(&self.roots[at].1, |it| &it.base)),
            None => None,
        };
        let mut known: FxHashMap<&Path, bool> = FxHashMap::default();
        let mut stale = Vec::new();
        for (at, (_, root)) in self.roots.iter().enumerate() {
            let changed = match &self.splits[at] {
                Some(split) => asked == Some(at) && !self.builds_alike(&split.base, root),
                None => match known.get(&**root) {
                    Some(&changed) => changed,
                    None => {
                        let env = (asked.is_none() || built == Some(root)).then_some(env);
                        let changed = self.inputs.stale(root, env)?;
                        known.insert(root, changed);
                        changed
                    }
                },
            };
            if changed {
                stale.push(at);
            }
        }
        Ok(stale)
    }

    /// Loads the checkouts at `stale` again, as a process with the variables `env` builds
    /// them. Every other keeps the workspace it has, and the crates that are the same in the
    /// workspace loaded again stay the crates they are. Nothing is changed when cargo turns
    /// one of them down. A checkout that shares packages with one of `stale` shares what is
    /// alike now, and is loaded in full when the two no longer build alike.
    fn reload(
        &mut self,
        stale: &[usize],
        env: &ide_db::base_db::Env,
        db: &mut RootDatabase,
        vfs: &mut vfs::Vfs,
    ) -> anyhow::Result<()> {
        let mut loaded = Vec::new();
        for &at in stale {
            loaded.push(self.loaded_again(at, env)?);
        }
        for (&at, (workspace, debug, config)) in stale.iter().zip(loaded) {
            self.place(at, workspace, debug, &config);
        }
        pause_mid_reload();
        let mut failed = Ok(());
        for at in 0..self.roots.len() {
            let Some(base) = self.splits[at].as_ref().map(|it| it.base.clone()) else { continue };
            let Some(base) = stale.iter().copied().find(|&it| self.roots[it].1 == base) else {
                continue;
            };
            let over = self.roots[at].1.clone();
            if !self.builds_alike(&self.roots[base].1, &over) {
                match self.loaded_again(at, env) {
                    Ok((workspace, debug, config)) => {
                        self.place(at, workspace, debug, &config);
                        continue;
                    }
                    Err(error) => failed = Err(error),
                }
            }
            let own = self.differing(base, &over);
            self.loaded.replace(at, self.split(base, &over, &own));
            self.splits[at] = Some(Split { base: self.roots[base].1.clone(), own });
            self.debug[at] = self.debug[base];
        }
        self.loaded.add(Vec::new(), &self.cargo_config.extra_env, &self.load_config, vfs, db)?;
        failed
    }

    /// The workspace of the checkout at `at` as cargo has it now for a process with the
    /// variables `env`, what a build of it has a debugger told, and how it was loaded.
    fn loaded_again(
        &self,
        at: usize,
        env: &ide_db::base_db::Env,
    ) -> anyhow::Result<(ProjectWorkspace, plan::Whole, CargoConfig)> {
        let (project, root) = &self.roots[at];
        let mut config = self.cargo_config.clone();
        config.extra_env.extend(self.inputs.variables(root, env));
        let (workspace, debug) =
            workspace_at(project, &config, &self.load_config, &self.selection)?;
        Ok((workspace, debug, config))
    }

    /// Has the checkout at `at` be `workspace`, loaded in full with `config`.
    fn place(
        &mut self,
        at: usize,
        workspace: ProjectWorkspace,
        debug: plan::Whole,
        config: &CargoConfig,
    ) {
        let root = PathBuf::from(workspace.workspace_root().to_string());
        self.inputs.watch(&root, &workspace, config);
        self.loaded.replace(at, workspace);
        self.roots[at].1 = root;
        self.splits[at] = None;
        self.debug[at] = debug;
    }
}

/// Waits for `RUSTD_RELOAD_PAUSE_MS` where a reload has the new workspaces of its checkouts
/// and the crates are still the old ones, and says so: what `harness/buildmodel.py` holds a
/// reload at while it sends the requests that must not see the two mixed.
fn pause_mid_reload() {
    let pause = std::env::var("RUSTD_RELOAD_PAUSE_MS").ok().and_then(|it| it.parse().ok());
    if let Some(pause) = pause {
        eprintln!("reload: paused");
        std::thread::sleep(Duration::from_millis(pause));
    }
}

/// The parts of a checkout that a build of it is told.
struct Parts {
    root: PathBuf,
    base: Option<PathBuf>,
    members: Arc<FxHashSet<ide_db::base_db::Crate>>,
    /// What the build has a debugger told.
    debug: plan::Described,
}

/// Whether the files of a package under `dir`, a directory of the checkout at `base`, are
/// those of the checkout at `over`. The packages in `nested` are not part of it,
/// and neither is what a build wrote.
fn package_alike(
    dir: &Path,
    base: &Path,
    over: &Path,
    nested: &FxHashSet<&Path>,
    compared: &Compared,
) -> bool {
    let Ok(inside) = dir.strip_prefix(base) else { return true };
    let theirs = over.join(inside);
    let skipped = |inside: &Path, name: &std::ffi::OsStr| {
        name == ".git"
            || inside.join(name).join("CACHEDIR.TAG").exists()
            || nested.contains(&*dir.join(name))
    };
    let names = |inside: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(inside)
            .ok()?
            .filter_map(|it| Some(it.ok()?.file_name()))
            .filter(|name| !skipped(inside, name))
            .collect();
        names.sort_unstable();
        Some(names)
    };
    let Some(ours) = names(dir) else { return false };
    names(&theirs).is_some_and(|it| it == ours)
        && ours.iter().all(|name| {
            let (path, their) = (dir.join(name), theirs.join(name));
            if path.is_dir() {
                return package_alike(&path, base, over, nested, compared);
            }
            let written = |path: &Path| {
                let file = std::fs::metadata(path).ok()?;
                Some((file.len(), file.modified().ok()?))
            };
            let (Some(ours), Some(theirs)) = (written(&path), written(&their)) else {
                return false;
            };
            if ours.0 != theirs.0 {
                return false;
            }
            let known = compared.lock().unwrap_or_else(|e| e.into_inner()).get(&their).copied();
            if let Some((_, _, alike)) = known.filter(|it| (it.0, it.1) == (ours, theirs)) {
                return alike;
            }
            let alike = match (std::fs::read(&path), std::fs::read(&their)) {
                (Ok(ours), Ok(theirs)) => {
                    // Code that asks where its package is does not say the same in two
                    // checkouts.
                    let asks = path.extension().is_some_and(|it| it == "rs")
                        && memchr::memmem::find_iter(&ours, b"env!(").any(|at| {
                            ours[at + 5..].trim_ascii_start().starts_with(b"\"CARGO_MANIFEST_DIR\"")
                        });
                    ours == theirs && !asks
                }
                _ => false,
            };
            compared.lock().unwrap_or_else(|e| e.into_inner()).insert(their, (ours, theirs, alike));
            alike
        })
}

/// Whether the files under `dir`, a directory of the checkout at `base`, are those of the
/// checkout at `over`.
fn tree_alike(dir: &Path, base: &Path, over: &Path) -> bool {
    let Ok(inside) = dir.strip_prefix(base) else { return true };
    let names = |dir: &Path| {
        let mut names: Vec<_> =
            std::fs::read_dir(dir).ok()?.filter_map(|it| Some(it.ok()?.file_name())).collect();
        names.sort_unstable();
        Some(names)
    };
    let theirs = over.join(inside);
    let Some(ours) = names(dir) else { return false };
    names(&theirs).is_some_and(|it| it == ours)
        && ours.iter().all(|name| {
            let path = dir.join(name);
            if path.is_dir() {
                tree_alike(&path, base, over)
            } else {
                std::fs::read(&path).ok() == std::fs::read(theirs.join(name)).ok()
            }
        })
}

/// A checkout that is a loaded one where its files are the same: the packages that are the
/// same in both are the crates of the loaded one, and only the others are crates of its own.
#[derive(Clone, PartialEq)]
struct Split {
    /// The root of the loaded checkout.
    base: PathBuf,
    /// The directories of the packages it has other files in, as they are called in both.
    own: BTreeSet<PathBuf>,
}

/// When a checkout that is not loaded is read as a loaded one with other files.
#[derive(Clone, Copy, PartialEq)]
enum Overlay {
    Never,
    /// For as long as taking in what differs is quick: the checkouts read as one are built
    /// in turn, so one that keeps taking long is loaded with its own crates instead.
    Quick,
    Always,
}

/// A checkout as the daemon looks at it.
struct View {
    /// The loaded checkout it is built as.
    base: PathBuf,
    /// Where its files are read from: `base`, or a checkout that is `base` with other contents.
    over: PathBuf,
    /// The project asked for, in `base`.
    project: PathBuf,
}

fn workspace_at(
    project: &Path,
    cargo_config: &CargoConfig,
    load_config: &LoadCargoConfig,
    selection: &plan::Selection,
) -> anyhow::Result<(ProjectWorkspace, plan::Whole)> {
    let root = AbsPathBuf::assert_utf8(std::env::current_dir()?.join(project));
    let manifest = ProjectManifest::discover_single(&root)?;
    let mut workspace = ProjectWorkspace::load(manifest.clone(), cargo_config, &|_| {})?;
    let mut debug = plan::Whole { debug: plan::Described::NOTHING, lto: false };
    if let ProjectWorkspaceKind::Cargo { cargo, .. } = &mut workspace.kind {
        if !selection.dev_deps {
            cargo.drop_dev_dependencies();
        }
        // `cargo metadata` unifies features over every member of the workspace and knows no
        // profile. The build of this one package has both.
        let dir = manifest.manifest_path().parent();
        let triple = project_model::toolchain_info::target_tuple::get(
            project_model::toolchain_info::QueryConfig::Rustc(&workspace.sysroot, dir.as_ref()),
            selection.target.as_deref(),
            &cargo_config.extra_env,
        )?;
        let triple = triple.first().context("no target to build for")?;
        let asked = plan::Asked { triple, cfg: &workspace.rustc_cfg, env: &cargo_config.extra_env };
        let plan = plan::Plan::of(manifest.manifest_path().as_ref(), selection, &asked)?;
        debug = plan.whole()?;
        let packages: Vec<_> = cargo.packages().collect();
        for package in packages {
            let Some(built) = plan.package(&cargo[package].id.repr) else { continue };
            cargo.set_active_features(package, built.features.iter().cloned().collect());
            cargo.set_profile_cfg(package, built.profile.cfg());
            cargo.set_primary(package, built.primary);
        }
    }
    if load_config.load_out_dirs_from_check {
        let build_scripts = workspace.run_build_scripts(cargo_config, &|_| {})?;
        workspace.set_build_scripts(build_scripts);
    }
    Ok((workspace, debug))
}

/// What the checkouts loaded have in common, and what holding them costs.
fn report_sharing(db: &RootDatabase) {
    let crates = Crate::all(db);
    let local = crates.iter().filter(|it| it.origin(db).is_local()).count();
    eprintln!(
        "sharing: {} crates, {local} of the checkouts' own, {} MB",
        crates.len(),
        memory::resident().unwrap_or(0)
    );
}

fn resolve_crates(db: &RootDatabase, args: &Args) -> anyhow::Result<usize> {
    let mut seen = FxHashSet::default();
    let mut crates = Vec::new();
    for name in args.crates() {
        for krate in crates_named(db, name, args)? {
            let krate: ide_db::base_db::Crate = krate.into();
            crates.extend(krate.transitive_deps(db).into_iter().filter(|&dep| seen.insert(dep)));
        }
    }
    // A crate comes before its dependencies in the list, and a thread that starts on one
    // waits for the threads working on what it depends on.
    crates.reverse();
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
                    let before = memory::live();
                    hir::attach_db(&db, || {
                        _ = hir::crate_def_map(&db, krate);
                        let names = start.elapsed();
                        _ = hir::TraitImpls::for_crate(&db, krate);
                        if std::env::var_os("RUSTD_STATS").is_some() {
                            let name = krate.extra_data(&db).display_name.as_ref();
                            let name = name.map(|it| it.to_string()).unwrap_or_default();
                            // What the heap grew by is this crate's when one thread resolves.
                            let grew = memory::live().zip(before).map(|(now, before)| {
                                let origin = &krate.data(&db).origin;
                                format!(", {:+} bytes {origin:?}", now as i64 - before as i64)
                            });
                            eprintln!(
                                "resolved {name}: names {names:.1?}, impls {:.1?}{}",
                                start.elapsed() - names,
                                grew.unwrap_or_default()
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
    functions: std::sync::Mutex<FxHashMap<Crate, std::sync::Arc<ListedCrate>>>,
    /// Every crate that was listed, also once an edit has it listed again.
    built: std::sync::Mutex<FxHashSet<Crate>>,
}

struct ListedCrate {
    functions: Vec<FunctionId>,
    /// For each, whether a runtime looks it up by name.
    exported: Vec<bool>,
    main: Option<FunctionId>,
}

/// Whether the root of `krate` says `#![no_main]`.
fn has_no_main(db: &RootDatabase, krate: Crate) -> bool {
    let root = ide_db::base_db::EditionedFileId::new(db, krate.root_file(db), krate.edition(db));
    let attrs = syntax::ast::HasAttrs::attrs(&root.parse(db).tree());
    attrs.filter_map(|it| it.as_simple_atom()).any(|it| it == "no_main")
}

impl ListedCrate {
    fn new(db: &RootDatabase, krate: Crate) -> ListedCrate {
        let functions = functions_of(db, krate);
        // A crate with `#![no_main]` has no `main` of Rust's: one it exports is the runtime's.
        let main = functions
            .iter()
            .copied()
            .find(|&it| hir::Function::from(it).is_main(db))
            .filter(|_| !has_no_main(db, krate));
        let exported = functions
            .iter()
            .map(|&it| exported_symbol(db, it).is_some() || serves_allocator(db, it))
            .collect();
        ListedCrate { functions, exported, main }
    }
}

/// Whether a function is a method of an `impl GlobalAlloc`. The program calls the allocator
/// through symbols rustc writes for the `#[global_allocator]`, so no call in the source names
/// these methods, and they run all the same.
fn serves_allocator(db: &RootDatabase, func: FunctionId) -> bool {
    let Some(item) = hir::Function::from(func).as_assoc_item(db) else { return false };
    let AssocItemContainer::Impl(implementation) = item.container(db) else { return false };
    implementation.trait_(db).is_some_and(|it| {
        let core = CrateOrigin::Lang(LangCrateOrigin::Core);
        it.name(db).as_str() == "GlobalAlloc" && it.module(db).krate(db).origin(db) == core
    })
}

impl Listed {
    /// Two builds that ask for a crate not listed yet both list it: neither waits for the other.
    fn of(&self, db: &RootDatabase, krate: Crate) -> std::sync::Arc<ListedCrate> {
        self.built.lock().unwrap().insert(krate);
        if let Some(listed) = self.functions.lock().unwrap().get(&krate) {
            return listed.clone();
        }
        let listed = std::sync::Arc::new(ListedCrate::new(db, krate));
        self.functions.lock().unwrap().entry(krate).or_insert(listed).clone()
    }

    /// Notes that a file changed: one of `crates`, or with `None` one we can't place.
    fn touch(&self, db: &RootDatabase, crates: Option<&[ide_db::base_db::Crate]>) {
        let Some(crates) = crates else {
            self.functions.lock().unwrap().clear();
            return;
        };
        // Asked before the lock is taken: a query that is cancelled unwinds, and would poison it.
        let seeing: Vec<Crate> = crates
            .iter()
            .flat_map(|krate| krate.transitive_rev_deps(db).into_iter().map(Crate::from))
            .collect();
        let mut functions = self.functions.lock().unwrap();
        for krate in seeing {
            functions.remove(&krate);
        }
    }
}

fn functions_of(db: &RootDatabase, krate: Crate) -> Vec<FunctionId> {
    let start = Instant::now();
    // The first thing asked of a crate after an edit: name resolution is redone here.
    hir_def::nameres::crate_def_map(db, krate.into());
    let resolved = start.elapsed();
    let mut declared = Declared::default();
    declared.in_modules(db, krate);
    let scopes = start.elapsed();
    let count = declared.bodies.len();
    // A body's blocks include those of the blocks in it, but not those of the items in them.
    while !declared.bodies.is_empty() {
        let round = lower(db, std::mem::take(&mut declared.bodies));
        for body in round {
            for (_, def_map) in Body::of(db, body).blocks(db) {
                for (_, module) in def_map.modules(db) {
                    declared.in_scope(db, module.scope(db));
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
    declared.functions
}

/// The functions of a crate that no body declares, as its modules are now. The ones listed
/// for a build are from before the edits since, and the bodies of those may be gone.
fn functions_of_modules(db: &RootDatabase, krate: Crate) -> Vec<FunctionId> {
    let mut declared = Declared::default();
    declared.in_modules(db, krate);
    declared.functions
}

/// What scopes declare that has code.
#[derive(Default)]
struct Declared {
    functions: Vec<FunctionId>,
    /// The bodies whose blocks declare more scopes.
    bodies: Vec<DefWithBodyId>,
}

impl Declared {
    fn in_modules(&mut self, db: &RootDatabase, krate: Crate) {
        let mut modules: Vec<Module> = vec![krate.root_module(db)];
        while let Some(module) = modules.pop() {
            modules.extend(module.children(db));
            let id = hir_def::ModuleId::from(module);
            self.in_scope(db, id.def_map(db).module(db, id).scope(db));
        }
    }

    fn in_scope(&mut self, db: &RootDatabase, scope: &ItemScope) {
        for def in scope.declarations() {
            match def {
                // A function declared in an `extern` block has no body to compile.
                ModuleDefId::FunctionId(id)
                    if !matches!(id.lookup(db).container, ItemContainerId::ExternBlockId(_)) =>
                {
                    self.functions.push(id);
                    self.bodies.push(id.into());
                }
                ModuleDefId::ConstId(id) => self.bodies.push(id.into()),
                ModuleDefId::StaticId(id) => self.bodies.push(id.into()),
                _ => {}
            }
        }
        self.bodies.extend(scope.unnamed_consts().map(DefWithBodyId::from));
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                match item {
                    AssocItemId::FunctionId(id) => {
                        self.functions.push(id);
                        self.bodies.push(id.into());
                    }
                    AssocItemId::ConstId(id) => self.bodies.push(id.into()),
                    AssocItemId::TypeAliasId(_) => {}
                }
            }
        }
    }
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
    /// Why rustc does not compile what was built, when `env!` asks for a variable nobody set.
    unset: Option<String>,
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
    listed: &Listed,
) -> anyhow::Result<Build> {
    let start = Instant::now();
    let contents = &content::Contents::default();
    let (root, base) = (args.checkout.clone(), args.base.clone());
    let debug = args.debug;
    let apart = (debug.kept != plan::Kept::Linked).then(|| args.out.clone());
    let resolver = Resolver {
        db,
        table,
        root,
        base,
        rustc_commit: Default::default(),
        contents,
        debug,
        apart,
        listed,
        shared_paths: Default::default(),
    };
    let hir_db: &dyn HirDatabase = db;
    let mut functions = Vec::new();
    // The `main` of the crate asked for: another program of the workspace among the crates
    // is only here for what this one uses of it.
    let mut main = None;
    let mut local = Vec::new();
    for (at, krate) in args.crates().enumerate() {
        for krate in crates_to_build(db, table, krate, args)? {
            local.push(krate);
            let listed = listed.of(db, krate);
            // With `cfg(test)` the program starts in the harness rustc wrote, which calls
            // every test: the crate's own `main` is one function among the others.
            if at == 0 && !args.cfg_test {
                main = main.or(listed.main);
            }
            functions.extend(listed.functions.iter().copied().zip(listed.exported.iter().copied()));
        }
    }
    let list = start.elapsed();
    // With a program to start from, only what it reaches is compiled, and what a runtime
    // looks up by name. Without one any function may be the one called.
    let (entries, on_demand): (Vec<_>, Vec<_>) = functions
        .into_iter()
        .partition(|&(func, exported)| exported || main.is_none() || main == Some(func));
    let unzip = |it: Vec<(FunctionId, bool)>| it.into_iter().map(|it| it.0).collect::<Vec<_>>();
    let (entries, on_demand) = (unzip(entries), unzip(on_demand));
    // The program's `main` is called by one rustc wrote, when rustc built the crate.
    let own = |main: &FunctionId| resolver.own_crate_id(hir_def::HasModule::krate(main, db));
    let roots = Roots {
        entries: &entries,
        on_demand: &on_demand,
        start: main.filter(|it| own(it).is_some()),
    };
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
    let mut unit =
        hir::attach_db(db, || CodegenUnit::compile(hir_db, &resolver, roots, &denied, known))
            .map_err(anyhow::Error::msg)?;
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
    if let Some(object) = &mut unit.object {
        // A patch defines again what the object does, and the linker of Mach-O takes two
        // definitions of a name only when one of them gives way.
        weaken::definitions(object);
        // The files a debugger is sent to are there before the object that sends it.
        for file in &unit.apart {
            replace(Path::new(&file.name), &file.bytes)?;
        }
        replace(&args.out, object)?;
    }
    if let (Some(path), Some(patch)) = (&args.patch, &unit.patch) {
        replace(path, patch)?;
    }
    let build = Build {
        compiled: unit.compiled.len(),
        skipped: unit.skipped.into_iter().map(|(func, e)| (name(func), e)).collect(),
        instances: unit.instances,
        codegen,
        emit: start.elapsed(),
        steps: [setup, revalidate - setup, codegen - revalidate],
        looked,
        unset: unset_in(db, &local)
            .map(|it| format!("`env!(\"{it}\")` names a variable that is not set")),
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
                let roots = Roots { entries: &[func], on_demand: &[], start: None };
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

/// The executables that edits are spliced into, by where each is. One program has as many as
/// there are checkouts read as its own, and a link may run after the build that asked for it.
static LINKED: std::sync::LazyLock<std::sync::Mutex<FxHashMap<PathBuf, Executable>>> =
    std::sync::LazyLock::new(Default::default);

/// Brings an executable up to date with its objects. An edit that left the whole object alone
/// is spliced into the executable at `splice`, which `--binary` names, when that is known from
/// its last link; anything else, and whatever the splice declines, runs the link command, whose
/// result is then known. `RUSTD_SPLICE=0` has every edit linked in full.
fn link(
    cmd: Option<&str>,
    splice: Option<&Path>,
    patch: Option<&Path>,
    object: &Path,
    patched: bool,
) -> anyhow::Result<()> {
    static SPLICING: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("RUSTD_SPLICE").is_none_or(|it| it != "0"));
    let splice = splice.filter(|_| *SPLICING);
    // An executable linked with the object as it was is not one to add to, also when the
    // object was written for a request that does not link.
    let rewritten = match splice {
        Some(binary) if !patched => LINKED.lock().unwrap().remove(binary).is_some(),
        _ => false,
    };
    let Some(cmd) = cmd else { return Ok(()) };
    let start = Instant::now();
    let mut declined = String::new();
    let running = splice.map_or(Ok(false), set_aside)?;
    if let (Some(binary), Some(patch)) = (splice, patch) {
        let known = LINKED.lock().unwrap().remove(binary).map(Executable::baseline);
        let spliced = match known {
            _ if rewritten => Err(anyhow::anyhow!("the whole object was written again")),
            _ if running => Err(anyhow::anyhow!("the executable is running")),
            Some(Ok(mut baseline)) if baseline.is_of(binary) => {
                let spliced = std::fs::read(patch)
                    .map_err(anyhow::Error::from)
                    .and_then(|patch| baseline.splice(&patch));
                spliced.map(|spliced| (baseline, spliced))
            }
            Some(Ok(_)) => Err(anyhow::anyhow!("another executable is asked for")),
            Some(Err(why)) => Err(why),
            None => Err(anyhow::anyhow!("no executable to add to")),
        };
        match spliced {
            Ok((baseline, Spliced { redirected, appended })) => {
                if redirected != 0 && baseline.leaves_debug_info() {
                    eprintln!(
                        "debug: the PDB is of the code before the splice: the {redirected} functions redirected have no lines in it until the next full link"
                    );
                }
                let read = Executable::Read(Box::new(baseline));
                LINKED.lock().unwrap().insert(binary.to_owned(), read);
                eprintln!(
                    "link: {:.1?} (spliced: {redirected} functions redirected, {appended} bytes)",
                    start.elapsed()
                );
                return Ok(());
            }
            Err(why) => declined = format!("; not spliced: {why:#}"),
        }
    }
    let status = shell(cmd).status()?;
    eprintln!("link: {:.1?} ({status}{declined})", start.elapsed());
    anyhow::ensure!(status.success(), "the link failed ({status})");
    if let (Some(binary), Some(patch)) = (splice, patch) {
        let linked = Executable::Linked(Linked::read(binary.to_owned(), patch, object));
        LINKED.lock().unwrap().insert(binary.to_owned(), linked);
    }
    Ok(())
}

/// Whether the executable at `binary` runs, in which case Windows lets nothing write it, and
/// it is moved aside: it runs on there, and the link writes a new one. It is deleted once it
/// has exited, by the first link to come after.
#[cfg(windows)]
fn set_aside(binary: &Path) -> anyhow::Result<bool> {
    const SHARING_VIOLATION: i32 = 32;
    let name = binary.file_name().context("an executable without a name")?.to_owned();
    let siblings = binary.parent().map(std::fs::read_dir).transpose()?.into_iter().flatten();
    for sibling in siblings {
        let sibling = sibling?;
        let is_aside = sibling.path().extension() == Some("running".as_ref())
            && sibling.file_name().as_encoded_bytes().starts_with(name.as_encoded_bytes());
        if is_aside {
            // The one that still runs stays.
            let _ = std::fs::remove_file(sibling.path());
        }
    }
    match std::fs::OpenOptions::new().write(true).open(binary) {
        Err(e) if e.raw_os_error() == Some(SHARING_VIOLATION) => {}
        _ => return Ok(false),
    }
    static ASIDE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let aside = ASIDE.fetch_add(1, Ordering::Relaxed);
    let mut moved = binary.as_os_str().to_owned();
    moved.push(format!(".{}-{aside}.running", std::process::id()));
    std::fs::rename(binary, &moved).context("moving the running executable aside")?;
    Ok(true)
}

/// Whether the executable at `binary` runs, which is nothing to a link on Unix: the linker
/// writes a new file in its place.
#[cfg(unix)]
fn set_aside(_: &Path) -> anyhow::Result<bool> {
    Ok(false)
}

/// The command that runs the line `cmd` as the platform's shell would.
#[cfg(unix)]
fn shell(cmd: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(cmd);
    shell
}

/// The command that runs the line `cmd`. It is the command line of the process as it is:
/// `cmd.exe` takes no line as long as the one a link has.
#[cfg(windows)]
fn shell(cmd: &str) -> Command {
    use std::os::windows::process::CommandExt as _;
    let cmd = cmd.trim_start();
    let (program, rest) = match cmd.strip_prefix('"').and_then(|it| it.split_once('"')) {
        Some(quoted) => quoted,
        None => cmd.split_once(char::is_whitespace).unwrap_or((cmd, "")),
    };
    let mut command = Command::new(program);
    command.raw_arg(rest);
    command
}

/// The executable of the last full link, for splicing edits into.
enum Executable {
    /// Just linked, and being read.
    Linked(Linked),
    Read(Box<Baseline>),
}

impl Executable {
    fn baseline(self) -> anyhow::Result<Baseline> {
        match self {
            Executable::Linked(linked) => linked.baseline(),
            Executable::Read(baseline) => Ok(*baseline),
        }
    }
}

/// The directories of the checkout at `base` that builds write to. They are not part of the
/// checkout: another checkout read in its place has the same ones.
fn built_in(db: &RootDatabase, base: &Path) -> Vec<PathBuf> {
    let crates = ide_db::base_db::all_crates(db);
    let scripts = crates.iter().filter_map(|krate| {
        let out = krate.env(db).get("OUT_DIR")?;
        let below = Path::new(&out).strip_prefix(base).ok()?.components().next()?;
        Some(base.join(below))
    });
    // Cargo tags the directories it builds in.
    let tagged = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .filter_map(|it| Some(it.ok()?.path()))
        .filter(|it| it.join("CACHEDIR.TAG").exists());
    let mut built: Vec<PathBuf> = scripts.chain(tagged).collect();
    built.sort_unstable();
    built.dedup();
    built
}

/// The variables of the process the build is for that `env!` and `option_env!` in `krate` have
/// asked for and that are set, sorted. Those the crate's own variables answer are not asked of
/// the process.
fn process_reads(db: &RootDatabase, krate: ide_db::base_db::Crate) -> Vec<(String, String)> {
    let env = krate.env(db);
    let mut reads: Vec<(String, String)> = db
        .env_read()
        .into_iter()
        .filter(|(of, key)| *of == krate && !env.contains_key(key))
        .filter_map(|(_, key)| {
            let value = ide_db::base_db::process_env_var(db, &key)?;
            Some((key, value))
        })
        .collect();
    reads.sort_unstable();
    reads
}

/// A variable `env!` asks for in one of `crates` that neither cargo nor the process the build
/// is for sets: rustc does not compile the crate.
fn unset_in(db: &RootDatabase, crates: &[Crate]) -> Option<String> {
    let mut unset: Vec<String> = db
        .env_unset()
        .into_iter()
        .filter(|(krate, key)| {
            crates.iter().any(|it| ide_db::base_db::Crate::from(*it) == *krate)
                && !krate.env(db).contains_key(key)
                && ide_db::base_db::process_env_var(db, key).is_none()
        })
        .map(|(_, key)| key)
        .collect();
    unset.sort_unstable();
    unset.into_iter().next()
}

/// The checkout a crate was loaded in, and the one its variables name now.
type Placed = FxHashMap<ide_db::base_db::Crate, (PathBuf, PathBuf)>;

/// Has the crates of the checkout at `base` that ask `env!` for a place in it be told the
/// place in `over`: `CARGO_MANIFEST_DIR` is where the checkout read is. The result is the
/// crates told another place than before.
fn place(
    db: &mut RootDatabase,
    base: &Path,
    over: &Path,
    placed: &mut Placed,
) -> Vec<ide_db::base_db::Crate> {
    let built = built_in(db, base);
    let mut asking: FxHashMap<_, Vec<String>> = FxHashMap::default();
    for (krate, key) in db.env_read() {
        asking.entry(krate).or_default().push(key);
    }
    let mut moved = Vec::new();
    for (krate, keys) in asking {
        let env = krate.env(db);
        let Some(manifest) = env.get("CARGO_MANIFEST_DIR") else { continue };
        let manifest = Path::new(&manifest);
        // A checkout that joins has the crates there are loaded again, as they first were.
        let from = match placed.get(&krate) {
            Some((of, from)) if of == base && manifest.starts_with(from) => from.clone(),
            Some((of, _)) if of != base => continue,
            _ if manifest.starts_with(base) => base.to_owned(),
            _ => continue,
        };
        if from == over {
            continue;
        }
        let moved_to = |value: &str| {
            let path = Path::new(value);
            let inside = path.strip_prefix(&from).ok()?;
            if built.iter().any(|it| path.starts_with(it)) {
                return None;
            }
            Some(if inside.as_os_str().is_empty() { over.to_owned() } else { over.join(inside) })
        };
        if !keys.iter().any(|key| env.get(key).is_some_and(|it| moved_to(&it).is_some())) {
            continue;
        }
        let mut new = env.clone();
        for (key, value) in env {
            if let Some(path) = moved_to(value) {
                new.set(key, path.to_string_lossy());
            }
        }
        db.set_env(krate, new);
        placed.insert(krate, (base.to_owned(), over.to_owned()));
        moved.push(krate);
    }
    if !moved.is_empty() {
        eprintln!("placed: {} crates in {}", moved.len(), over.display());
    }
    moved
}

/// The arguments to cargo that have it run the build scripts of the build `args` asks for.
fn selected(args: &Args) -> Vec<String> {
    let selection = &args.selection;
    let mut selected = vec!["--profile".to_owned(), selection.profile.clone()];
    for package in &selection.packages {
        selected.extend(["--package".to_owned(), package.clone()]);
    }
    if selection.workspace {
        selected.push("--workspace".to_owned());
    }
    selected
}

/// Has `bytes` be the file at `path`. A file that was there is left as it is for whoever has
/// it open or under another name: the object of a program can be that of another one too.
fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut new = path.as_os_str().to_owned();
    new.push(format!(".{}.new", std::process::id()));
    std::fs::write(&new, bytes)?;
    std::fs::rename(&new, path)
}

/// What cargo said of every crate when the checkouts were last loaded.
struct CratesAsLoaded(FxHashMap<ide_db::base_db::Crate, CrateAsLoaded>);

type CrateAsLoaded = (
    ide_db::base_db::BuiltCrateData,
    ide_db::base_db::ExtraCrateData,
    cfg::CfgOptions,
    ide_db::base_db::Env,
);

impl CratesAsLoaded {
    fn of(db: &RootDatabase) -> CratesAsLoaded {
        let crates = ide_db::base_db::all_crates(db);
        CratesAsLoaded(crates.iter().map(|&it| (it, Self::said(db, it))).collect())
    }

    fn said(db: &RootDatabase, krate: ide_db::base_db::Crate) -> CrateAsLoaded {
        (
            krate.data(db).clone(),
            krate.extra_data(db).clone(),
            krate.cfg_options(db).clone(),
            krate.env(db).clone(),
        )
    }

    /// The crates of `db` that cargo says something else of now, or that were not there.
    fn changed(&self, db: &RootDatabase) -> Vec<ide_db::base_db::Crate> {
        let crates = ide_db::base_db::all_crates(db);
        let same = |krate| self.0.get(&krate).is_some_and(|was| *was == Self::said(db, krate));
        crates.iter().copied().filter(|&it| !same(it)).collect()
    }
}

/// The source files the daemon watches.
struct Sources {
    paths: FxHashMap<vfs::FileId, PathBuf>,
}

/// Why the source as it is can't be built by the daemon, when it can't.
fn refusal(
    db: &RootDatabase,
    sources: &[vfs::FileId],
    watched: &Sources,
    out_of_date: Option<(FunctionId, String)>,
) -> Option<String> {
    let Sources { paths } = watched;
    for &file in sources {
        let path = paths.get(&file);
        // A manifest is cargo's to read: the checkout is loaded again when one changes.
        if path.is_some_and(|it| it.file_name().is_some_and(|it| it == "Cargo.toml")) {
            continue;
        }
        let edition = ide_db::base_db::relevant_crates(db, file)
            .first()
            .map_or(syntax::Edition::CURRENT, |it| it.data(db).edition);
        let parse = ide_db::base_db::EditionedFileId::new(db, file, edition).parse(db);
        if let Some(error) = parse.errors().first() {
            let text = db.file_text(file).text(db);
            // A file that is included where an expression goes is not one of items.
            if syntax::ast::Expr::parse(text, edition).errors().is_empty() {
                continue;
            }
            let path = path.map(|it| it.display().to_string()).unwrap_or_default();
            let line = text[..usize::from(error.range().start())].matches('\n').count();
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

/// The text of the source file at `path` as the database has it.
fn source_on_disk(path: &Path) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    let rust = path.extension().is_some_and(|it| it == "rs");
    Ok(if rust { load_cargo::source_text(text) } else { text })
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
    /// Whether the file is never written again: everything is watched but the sysroot, the
    /// registry and what a build made, as path dependencies live outside the project. What a
    /// build script wrote is written again when it runs again.
    fn immutable(path: &Path) -> bool {
        let names: Vec<&str> = path.components().filter_map(|c| c.as_os_str().to_str()).collect();
        let of_script = names.windows(3).any(|it| it[0] == "build" && it[2] == "out");
        names.iter().any(|&it| matches!(it, "rustlib" | "registry"))
            || names.contains(&"target") && !of_script
    }

    /// `path` is what the file is known as, `on_disk` where it is read from.
    fn new(file: Watched, path: &Path, on_disk: &Path) -> Option<WatchedFile> {
        if WatchedFile::immutable(path) {
            return None;
        }
        let mtime = std::fs::metadata(on_disk).ok()?.modified().ok()?;
        Some(WatchedFile { file, path: path.to_owned(), mtime })
    }

    /// The files of the VFS that have a text. One that is not UTF-8 has none, and is no
    /// source of anything.
    fn sources(vfs: &Vfs, db: &RootDatabase) -> Vec<WatchedFile> {
        vfs.iter()
            .filter(|(id, _)| db.has_text(*id))
            .filter_map(|(id, path)| {
                let path: &Path = path.as_path()?.as_ref();
                WatchedFile::new(Watched::Source(id), path, path)
            })
            .collect()
    }

    /// Puts the files an `include!` named that were not loaded in their source roots, and
    /// watches them: whether there were any.
    fn include_sources(
        files: &mut Vec<WatchedFile>,
        tree: &mut tree::Tree,
        db: &mut RootDatabase,
    ) -> bool {
        let added = tree.included(db);
        for (file, path) in &added {
            files.extend(WatchedFile::new(Watched::Source(*file), path, &db.on_disk(path)));
        }
        !added.is_empty()
    }

    /// Adds the files the last build included that are not watched yet. One is read again once
    /// its timestamp is taken, so a write between the build's read and now is not lost: the
    /// result is whether there was one.
    fn unwatched(files: &[WatchedFile], db: &RootDatabase) -> bool {
        let watched: FxHashSet<&Path> = files.iter().map(|it| &*it.path).collect();
        db.included_files()
            .iter()
            .map(AsRef::<Path>::as_ref)
            .any(|it| !watched.contains(it) && !WatchedFile::immutable(it))
    }

    fn watch_included(files: &mut Vec<WatchedFile>, db: &mut RootDatabase) -> bool {
        let watched: FxHashSet<&Path> = files.iter().map(|it| &*it.path).collect();
        let new: Vec<AbsPathBuf> = db
            .included_files()
            .into_iter()
            .filter(|it| !watched.contains(AsRef::<Path>::as_ref(it)))
            .collect();
        let mut changed = false;
        for path in new {
            let on_disk = db.on_disk(path.as_ref());
            if let Some(file) =
                WatchedFile::new(Watched::Included(path.clone()), path.as_ref(), &on_disk)
            {
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
    let crates: Vec<_> =
        std::iter::once(args.krate.clone()).chain(args.batch.iter().cloned()).collect();
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
            krate: krate.clone(),
            batch: Vec::new(),
            widened: Vec::new(),
            out: out.join(format!("{krate}.o")),
            symbol_files: Vec::new(),
            watch: false,
            socket: None,
            link_cmd: None,
            patch: None,
            splice: None,
            report: Some(report.join(format!("{krate}.tsv"))),
            deny: None,
            isolate: None,
            cfg_test: args.cfg_test,
            own_target: args.own_target,
            selection: args.selection.clone(),
            also: Vec::new(),
            debug: args.debug,
            checkout: args.checkout.clone(),
            base: args.base.clone(),
            members: args.members.clone(),
        };
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build(db, table, &args, &mut Rejections::default(), &mut Listed::default())
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
    /// Say which tests of a program the changes in a file the client wrote reach, as the
    /// program was last built.
    Select,
}

impl Request {
    fn parse(line: &str) -> Option<Request> {
        match line {
            "sync" => Some(Request::Sync),
            "patch" => Some(Request::Patch),
            "select" => Some(Request::Select),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Request::Sync => "sync",
            Request::Patch => "patch",
            Request::Select => "select",
        }
    }
}

/// The program of the workspace a request is for, and where its objects go.
#[derive(Clone)]
struct Target {
    /// The checkout it is a program of. `None` is the one the daemon was started in.
    project: Option<PathBuf>,
    krate: String,
    /// The other crates of the workspace that are in it, when the client says: the names
    /// after the first in `--crate a,b,c`. Otherwise the ones the daemon was started with.
    batch: Option<Vec<String>>,
    out: PathBuf,
    patch: Option<PathBuf>,
    link_cmd: Option<String>,
    splice: Option<PathBuf>,
    /// What its link takes that rustc compiled. Code rustc instantiated for one program is in
    /// that program's own objects, so another has to be told where its copies are. Empty for
    /// the files the daemon was started with.
    symbol_files: Vec<PathBuf>,
}

impl Target {
    fn is(&self, args: &Args) -> bool {
        self.krate == args.krate && self.out == args.out
    }

    /// The daemon's arguments, were it started for this program.
    fn args(self, daemon: &Args, checkouts: &Checkouts) -> anyhow::Result<Args> {
        let Target { project, krate, batch, out, patch, link_cmd, splice, symbol_files } = self;
        let project = project.unwrap_or_else(|| daemon.project.clone());
        let Parts { root: checkout, base, members, debug } = checkouts.parts(&project)?;
        let symbol_files =
            if symbol_files.is_empty() { daemon.symbol_files.clone() } else { symbol_files };
        let batch = batch.unwrap_or_else(|| {
            let started_with = std::iter::once(&daemon.krate).chain(&daemon.batch);
            started_with.filter(|&it| *it != krate).cloned().collect()
        });
        Ok(Args {
            project,
            checkout,
            base,
            members,
            debug,
            krate,
            batch,
            widened: Vec::new(),
            out,
            patch,
            link_cmd,
            splice,
            symbol_files,
            ..daemon.clone()
        })
    }

    /// From what follows the socket on the command line of a client.
    fn from_flags(mut flags: impl Iterator<Item = String>) -> anyhow::Result<Option<Target>> {
        let (mut krate, mut out, mut patch, mut link_cmd) = (None, None, None, None);
        let (mut project, mut splice) = (None, None);
        let mut symbol_files = Vec::new();
        while let Some(flag) = flags.next() {
            let value = flags.next().with_context(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--crate" => krate = Some(value),
                "--project" => project = Some(PathBuf::from(value)),
                "--out" => out = Some(PathBuf::from(value)),
                "--patch" => patch = Some(PathBuf::from(value)),
                "--link-cmd" => link_cmd = Some(value),
                "--binary" | "--splice" => splice = Some(PathBuf::from(value)),
                "--symbols" => symbol_files.push(PathBuf::from(value)),
                _ => bail!("unexpected argument {flag}"),
            }
        }
        match (krate, out) {
            (Some(krate), Some(out)) => {
                let (krate, batch) = Target::crates(&krate);
                Ok(Some(Target {
                    project,
                    krate,
                    batch,
                    out,
                    patch,
                    link_cmd,
                    splice,
                    symbol_files,
                }))
            }
            (None, None)
                if patch.is_none()
                    && link_cmd.is_none()
                    && splice.is_none()
                    && symbol_files.is_empty()
                    && project.is_none() =>
            {
                Ok(None)
            }
            _ => bail!("a program is named by --crate and --out together"),
        }
    }

    /// The fields after the request's name on its line, each after a tab. Empty is absent.
    fn parse(fields: &[&str]) -> Option<Target> {
        let &[project, krate, out, patch, link_cmd, splice, ref symbol_files @ ..] = fields else {
            return None;
        };
        let given = |it: &str| (!it.is_empty()).then(|| it.to_owned());
        let (krate, batch) = Target::crates(krate);
        Some(Target {
            project: given(project).map(PathBuf::from),
            krate,
            batch,
            out: PathBuf::from(out),
            patch: given(patch).map(PathBuf::from),
            link_cmd: given(link_cmd),
            splice: given(splice).map(PathBuf::from),
            symbol_files: symbol_files.iter().map(PathBuf::from).collect(),
        })
    }

    /// The program's crate and the others in it, from `a,b,c`. A name with no comma says
    /// nothing of the others, and `a,` says there are none.
    fn crates(named: &str) -> (String, Option<Vec<String>>) {
        match named.split_once(',') {
            Some((krate, batch)) => {
                let batch = batch.split(',').filter(|it| !it.is_empty()).map(str::to_owned);
                (krate.to_owned(), Some(batch.collect()))
            }
            None => (named.to_owned(), None),
        }
    }

    fn fields(&self) -> String {
        let crates = match &self.batch {
            Some(batch) => format!("{},{}", self.krate, batch.join(",")),
            None => self.krate.clone(),
        };
        let mut fields = format!(
            "\t{}\t{}\t{}\t{}\t{}\t{}",
            self.project.as_ref().map(|it| it.display().to_string()).unwrap_or_default(),
            crates,
            self.out.display(),
            self.patch.as_ref().map(|it| it.display().to_string()).unwrap_or_default(),
            self.link_cmd.as_deref().unwrap_or_default(),
            self.splice.as_ref().map(|it| it.display().to_string()).unwrap_or_default()
        );
        for file in &self.symbol_files {
            fields.push('\t');
            fields.push_str(&file.display().to_string());
        }
        fields
    }
}

/// What the daemon keeps for a program it builds. The code compiled for one program is there
/// for the others: only the objects and what went into them are its own.
struct Program {
    args: Args,
    table: Arc<SymbolTable>,
    known: Rejections,
    unbuilt: Unbuilt,
    /// The object the last build was asked for.
    wrote: Option<PathBuf>,
    /// The object with all of the program, which a patch is the difference from.
    whole: Option<PathBuf>,
    /// How many of its builds a change to the source has interrupted since one finished.
    interrupted: u32,
    /// Whether its crates are others than the last build was of, which is then no patch.
    moved: bool,
    /// The tests of its crate when it was first built, which is how rustc built it.
    tests: Option<Tests>,
}

/// The functions of a program's own crate that a test harness runs, each with its
/// attributes. The harness of a test executable is rustc's: it lists the tests by name and
/// by what `#[ignore]` and `#[should_panic]` say of them, so a build after the list changed
/// would run the tests of before.
#[derive(PartialEq)]
struct Tests(Vec<(String, String)>);

impl Tests {
    fn of(db: &RootDatabase, listed: &Listed, args: &Args) -> Tests {
        use hir::HasSource as _;
        use syntax::ast::HasAttrs as _;
        let crates = crates_named(db, &args.krate, args).unwrap_or_default();
        let mut tests = Vec::new();
        for krate in crates {
            for &id in &listed.of(db, krate).functions {
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
                let attrs = function.source(db).map(|it| {
                    let attrs = it.value.attrs().map(|it| it.syntax().text().to_string());
                    attrs.collect::<Vec<_>>().join(" ")
                });
                tests.push((path.join("::"), attrs.unwrap_or_default()));
            }
        }
        tests.sort();
        Tests(tests)
    }

    /// Why the program can't be built by the daemon, when its tests are not these.
    fn changed(&self, now: &Tests) -> Option<String> {
        if self == now {
            return None;
        }
        let differs =
            |a: &Tests, b: &Tests| a.0.iter().find(|it| !b.0.contains(it)).map(|it| it.0.clone());
        let which = differs(now, self).or_else(|| differs(self, now)).unwrap_or_default();
        Some(format!(
            "the test `{which}` was added, removed or given other attributes, and the list of \
             tests is rustc's to write"
        ))
    }
}

/// What of the program `target` names to run for the changes `client` wrote: of the
/// program as it was last built, which the client asked for before.
fn picked(
    db: &RootDatabase,
    programs: &[Program],
    target: Option<&Target>,
    client: &Client,
    files: &[WatchedFile],
) -> impact::Picked {
    let program = target.and_then(|target| programs.iter().find(|it| target.is(&it.args)));
    let Some(program) = program else {
        return impact::Picked::Whole("the daemon has not built it".to_owned());
    };
    let read = client.changes.as_ref().map(|path| {
        let text = std::fs::read(path)?;
        anyhow::Ok(serde_json::from_slice::<Vec<impact::Change>>(&text)?)
    });
    match read {
        Some(Ok(changes)) => impact::select(db, &program.args, files, &changes),
        Some(Err(why)) => impact::Picked::Whole(format!("reading the changes: {why:#}")),
        None => impact::Picked::Whole("no changes were named".to_owned()),
    }
}

/// A request, the program it is for, and who waits for the answer.
type Asked = (Request, Option<Target>, Client);

/// Who asked for a build, and the variables of its process.
struct Client {
    stream: UnixStream,
    env: ide_db::base_db::Env,
    /// For a request to select tests, the file the client wrote the changes to.
    changes: Option<Box<Path>>,
}

impl std::io::Write for &Client {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (&self.stream).write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        (&self.stream).flush()
    }
}

/// What the daemon waits for.
enum Event {
    Asked(Box<Asked>),
    /// The build of a program is over, with what to tell its clients: nothing when a change
    /// to the source interrupted it.
    Built(usize, Box<Program>, Option<anyhow::Result<Result<String, String>>>),
    /// The link of a program's last build failed, after the build was over.
    Unlinked(usize),
}

/// Set while the builds under way are told to stop for a change to the source.
static INTERRUPTING: AtomicBool = AtomicBool::new(false);

/// Reading a loaded checkout from another place is long when the build takes more than this,
/// or than `RUSTD_SLOW_SWITCH_MS`.
const SLOW_SWITCH: Duration = Duration::from_secs(2);

/// A checkout that is long to turn to or from this many times running is loaded instead.
const SLOW_SWITCHES: u32 = 3;

/// A build that is interrupted this many times is waited for the next time.
const INTERRUPTIONS: u32 = 3;

/// The clients of a program that is being built.
struct Running {
    clients: Vec<Asked>,
    /// Whether a change to the source waits for the build: one that patches is short, and
    /// has nothing to patch once it is interrupted.
    kept: bool,
}

/// The builds under way and the requests that wait for them. A build reads the database on
/// threads of its own, so requests for other programs are not kept waiting by it.
struct Builds {
    events: std::sync::mpsc::Receiver<Event>,
    running: FxHashMap<usize, Running>,
    /// Requests that could not be built with the ones before them.
    waiting: std::collections::VecDeque<Asked>,
    /// Whether the requests that wait can only be built once something else happens.
    stuck: bool,
}

impl Builds {
    /// Queues a request, or takes back the program of a build that is over and answers its
    /// clients. Those of a build that was interrupted ask again, ahead of the others.
    fn take(&mut self, event: Event, programs: &mut [Program]) -> anyhow::Result<()> {
        self.stuck = false;
        let (at, program, answer) = match event {
            Event::Asked(asked) => {
                self.waiting.push_back(*asked);
                return Ok(());
            }
            Event::Built(at, program, answer) => (at, program, answer),
            Event::Unlinked(at) => {
                programs[at].unbuilt.stale = true;
                programs[at].wrote = None;
                return Ok(());
            }
        };
        let Some(Running { clients, .. }) = self.running.remove(&at) else { return Ok(()) };
        programs[at] = *program;
        let program = &mut programs[at];
        let Some(answer) = answer else {
            eprintln!("interrupted: {}", program.args.krate);
            let patching = program.known.patching;
            program.known = Rejections::default();
            program.known.patching = patching;
            program.unbuilt.stale = true;
            (program.wrote, program.whole) = (None, None);
            program.interrupted += 1;
            clients.into_iter().rev().for_each(|it| self.waiting.push_front(it));
            return Ok(());
        };
        (program.interrupted, program.moved) = (0, false);
        let answer = answer?;
        for (_, _, client) in clients {
            // A client that hangs up before the answer is not the daemon's problem.
            let _ = match &answer {
                Ok(built) => writeln!(&client, "ok {built}"),
                Err(why) => writeln!(&client, "error: {}", why.replace('\n', " ")),
            };
        }
        Ok(())
    }

    /// Leaves no build under way, for the database to be written to: a write stops every
    /// thread that reads it. The builds that are kept are waited for, the others interrupted.
    fn settle(&mut self, db: &mut RootDatabase, programs: &mut [Program]) -> anyhow::Result<()> {
        use ide_db::base_db::salsa::Database as _;
        while self.running.values().any(|it| it.kept) {
            let event = self.events.recv()?;
            self.take(event, programs)?;
        }
        if self.running.is_empty() {
            return Ok(());
        }
        INTERRUPTING.store(true, Ordering::SeqCst);
        db.trigger_cancellation();
        while !self.running.is_empty() {
            let event = self.events.recv()?;
            self.take(event, programs)?;
        }
        INTERRUPTING.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// Where a build of a program goes. A checkout looked at as another has files of its own for
/// the program they share.
#[derive(Clone, PartialEq)]
struct Outputs {
    out: PathBuf,
    patch: Option<PathBuf>,
    link_cmd: Option<String>,
    splice: Option<PathBuf>,
}

impl Outputs {
    fn of(args: &Args) -> Outputs {
        Outputs {
            out: args.out.clone(),
            patch: args.patch.clone(),
            link_cmd: args.link_cmd.clone(),
            splice: args.splice.clone(),
        }
    }
}

/// What a program's objects are behind by.
#[derive(Default)]
struct Unbuilt {
    /// Files edited since the last build that was not refused.
    edited: Vec<vfs::FileId>,
    /// Whether there is anything to build even when no file has changed since.
    stale: bool,
    /// Whether the last build was for a request that does not link, which leaves the
    /// executable to the next that does.
    unlinked: bool,
    /// Whether a client has named what rustc built of the checkout since the edits, as one
    /// does when the checkout is loaded again: the crates left to rustc are then as edited.
    named_anew: bool,
}

/// Sends `request` to the daemon listening on `socket` and waits for it. The answer is one
/// line: `ok unchanged`, or `ok patch=PATH ...` when the object at PATH holds the functions that
/// differ from the first build's, or `ok object=PATH ...` when the whole object was written
/// again.
fn ask(request: Request, socket: &Path, target: Option<Target>) -> anyhow::Result<()> {
    let mut stream =
        socket::connect(socket).with_context(|| format!("no daemon at {}", socket.display()))?;
    let target = target.map(|it| it.fields()).unwrap_or_default();
    let env = process::encode(&process::own());
    stream.write_all(format!("{}{target}\n{env}\n", request.name()).as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let reply = reply.trim_end();
    anyhow::ensure!(reply.starts_with("ok"), "daemon: {reply}");
    println!("{reply}");
    Ok(())
}

/// Loads the checkout of `project` into the database of a daemon that is running, and
/// watches its files.
fn join(
    project: &Path,
    checkouts: &mut Checkouts,
    db: &mut RootDatabase,
    tree: &mut tree::Tree,
    files: &mut Vec<WatchedFile>,
) -> anyhow::Result<()> {
    checkouts.add(&[project], db, tree.vfs())?;
    watch_loaded(db, tree, files);
    Ok(())
}

/// Watches the source files that were loaded since the last were.
fn watch_loaded(db: &mut RootDatabase, tree: &mut tree::Tree, files: &mut Vec<WatchedFile>) {
    let watched: FxHashSet<vfs::FileId> = files
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some(id),
            Watched::Included(_) => None,
        })
        .collect();
    let new: Vec<WatchedFile> = WatchedFile::sources(tree.vfs(), db)
        .into_iter()
        .filter(|it| !matches!(it.file, Watched::Source(id) if watched.contains(&id)))
        .collect();
    let sources: Vec<_> = new
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some((id, &*it.path)),
            Watched::Included(_) => None,
        })
        .collect();
    tree.watch(db, &sources);
    files.extend(new);
}

/// Reads the request a client sent on `stream`, and the variables of its process: the
/// result is where to send the outcome. One that is not a request is told so and is `None`.
fn requested(stream: UnixStream) -> Option<Asked> {
    let [request, env] = match socket::read_request_and_env(&stream) {
        Ok(lines) => lines,
        Err(error) => {
            // A client that hangs up before the answer is not the daemon's problem.
            let _ = writeln!(&stream, "error: {error}");
            return None;
        }
    };
    let mut fields: Vec<&str> = request.split('\t').collect();
    let changes = match Request::parse(fields[0]) {
        Some(Request::Select) if fields.len() > 1 => Some(Path::new(fields.remove(1)).into()),
        _ => None,
    };
    let target = Target::parse(&fields[1..]);
    if let Some(request) = Request::parse(fields[0])
        && (fields.len() == 1 || target.is_some())
        && (request != Request::Select || target.is_some())
    {
        let client = Client { stream, env: process::decode(&env), changes };
        return Some((request, target, client));
    }
    let _ = writeln!(&stream, "error: unknown request {request:?}");
    None
}

/// Ends the process when its standard input ends: the daemon that started this worker holds
/// the other end for as long as it lives, however it dies.
fn end_with_input() {
    let mut held = [0u8; 64];
    while std::io::Read::read(&mut std::io::stdin(), &mut held).is_ok_and(|read| read > 0) {}
    std::process::exit(0);
}

const HELP: &str = "\
rustd: a daemon that keeps Rust workspaces compiled and builds edits from memory

usage: rustd <command>

  enable            plain `cargo build|run|test|check` go through rustd from now on
  disable           cargo's configuration is put back as it was
  status            whether rustd is enabled, what the daemon has loaded, what fell back
  stop              ends the daemon; the next build starts one
  cargo <command>   what `cargo rustd <command>` runs: build, run, test, check
  cache <verb>      the disk store: stats, gc, verify, push, pull, keygen
  --help, --version

`cargo rustd build|run|test|check` takes cargo's flags. CARGO.md is the guide.";

/// Has a command that prints for a reader end quietly when the reader stops reading,
/// instead of failing on the closed pipe. Not for a process that writes to sockets.
pub(crate) fn end_with_reader() {
    #[cfg(unix)]
    // SAFETY: restores the default action of a signal, before any thread is started.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn main() -> anyhow::Result<()> {
    let stands_in_for_rustc = std::env::var_os("RUSTD_WRAPS_RUSTC").is_some();
    match std::env::args().nth(1).as_deref() {
        _ if stands_in_for_rustc => {}
        None | Some("--help" | "-h" | "help") => {
            end_with_reader();
            println!("{HELP}");
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("rustd {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("cache") => {
            end_with_reader();
            return remote::command(std::env::args().skip(2));
        }
        _ => {}
    }
    #[cfg(windows)]
    if let Some(script) = cargo::script::stood_in_front_of() {
        return cargo::script::run_as_cargo_does(script, std::env::args_os().skip(1));
    }
    // A build script runs with the variables of the cargo that runs it, the wrapper's too.
    if std::env::args().nth(1).as_deref() == Some("build-script") {
        return cargo::script::command(std::env::args_os().skip(2));
    }
    match std::env::args().nth(1).as_deref() {
        Some("rustc-wrapper") => return cargo::wrapper::command(std::env::args_os().skip(2)),
        Some("cargo") => return cargo::command(std::env::args().skip(2)),
        Some("enable" | "disable" | "status" | "stop") => {
            return cargo::command(std::env::args().skip(1));
        }
        Some("daemon") => return cargo::daemon::serve(),
        _ => {}
    }
    if std::env::var_os(cargo::daemon::ENDS_WITH_INPUT).is_some() {
        std::thread::spawn(end_with_input);
    }
    if std::env::var_os("RUSTD_WRAPS_RUSTC").is_some() {
        return stub::command(std::env::args_os().skip(1));
    }
    if std::env::args().nth(1).as_deref() == Some("deps") {
        return deps::command(std::env::args().skip(2));
    }
    if std::env::args().nth(1).as_deref() == Some("weaken") {
        return weaken::command(std::env::args().skip(2));
    }
    if let Some((request, socket)) =
        std::env::args().nth(1).and_then(|it| Request::parse(&it)).zip(std::env::args().nth(2))
    {
        return ask(request, Path::new(&socket), Target::from_flags(std::env::args().skip(3))?);
    }
    if std::env::var_os(doctest::ENABLED).is_some() {
        return doctest::command(std::env::args_os().skip(1));
    }
    run(Args::parse(std::env::args().skip(1))?, None)
}

/// Loads the workspace `args` names and builds what they ask for, then serves it when they
/// ask for that. With `doc` the build is of a library whose examples are then run.
fn run(mut args: Args, doc: Option<&doctest::Asked>) -> anyhow::Result<()> {
    // As cargo does: the flag, then the environment, then the host.
    if args.selection.target.is_none() {
        args.selection.target =
            std::env::var("CARGO_BUILD_TARGET").ok().filter(|it| !it.is_empty());
    }
    let emits = match &args.selection.target {
        Some(triple) => hir_ty::mir::codegen::Target::select(triple),
        None => hir_ty::mir::codegen::Target::selected(),
    };
    let emits = emits.map_err(anyhow::Error::msg)?;
    if args.selection.target.is_some() {
        eprintln!("target: {}", emits.triple());
    }
    let backend = Backend::from_env().map_err(anyhow::Error::msg)?;
    if backend != Backend::Cranelift {
        eprintln!("backend: {backend}");
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

    // What other machines compiled is fetched next to the workspace too.
    let shared = std::thread::spawn(|| {
        let shared = remote::Shared::from_env();
        if let Some(shared) = &shared {
            shared.pull();
        }
        shared
    });

    let start = Instant::now();
    // The library is read as the toolchain that built the program has it: one the directory
    // would pick now may be another, whose library has names the built one does not.
    let built_with = args.symbol_files.iter().find_map(|file| {
        let name = file.file_name()?.to_str()?;
        let (toolchain, _) = file.to_str()?.split_once("/lib/rustlib/")?;
        (name.starts_with("libstd-") && name.ends_with(".rlib")).then_some(toolchain)
    });
    let sysroot = match built_with.map(AbsPathBuf::try_from) {
        Some(Ok(toolchain)) => RustLibSource::Path(toolchain),
        _ => RustLibSource::Discover,
    };
    let cargo_config = CargoConfig {
        // The variables of whoever asks for a build: those of crates are read as rustc does.
        extra_env_for_commands_only: true,
        sysroot: Some(sysroot),
        target: args.selection.target.clone(),
        set_test: args.cfg_test,
        // Tests, benches and examples are what dev-dependencies are for: cargo runs the
        // build scripts and the proc macros of those only for a build that has them.
        all_targets: args.cfg_test || args.selection.dev_deps,
        features: match args.selection.all_features {
            true => CargoFeatures::All,
            false => CargoFeatures::Selected {
                features: args.selection.features.clone(),
                no_default_features: args.selection.no_default_features,
            },
        },
        // The build scripts say what the build asked for says, not what `cargo check` of the
        // workspace does.
        extra_args: selected(&args),
        only_manifest_package: true,
        target_dir_config: match args.own_target {
            true => TargetDirectoryConfig::UseSubdirectory,
            false => TargetDirectoryConfig::None,
        },
        ..Default::default()
    };
    let selection = args.selection.clone();
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };
    let projects: Vec<&Path> =
        std::iter::once(&args.project).chain(&args.also).map(PathBuf::as_path).collect();
    let (mut db, vfs, mut checkouts) = load(&projects, &cargo_config, load_config, selection)?;
    ide_db::base_db::set_process_env(&mut db, process::own());
    match toolchain_identity(&vfs, &args.symbol_files) {
        Some(toolchain) => hir_ty::mir::codegen::compiles_against(&toolchain),
        None => eprintln!("store: the toolchain of the library cannot be told from another"),
    }
    let Parts { root, base, members, debug } = checkouts.parts(&args.project)?;
    (args.checkout, args.base, args.members, args.debug) = (root, base, members, debug);
    eprintln!("load workspace: {:.1?}", start.elapsed());
    memory::checkpoint("load");

    if args.out.is_dir() {
        return build_batch(&db, args);
    }

    let mut files = WatchedFile::sources(&vfs, &db);
    let sources: Vec<_> = files
        .iter()
        .filter_map(|it| match it.file {
            Watched::Source(id) => Some((id, &*it.path)),
            Watched::Included(_) => None,
        })
        .collect();
    let mut tree = tree::Tree::new(&db, vfs, &sources);

    let start = Instant::now();
    if checkouts.lto(&args.project)? {
        linked_from_libraries(&db, &args)?;
    }
    let mut resolved = resolve_crates(&db, &args)?;
    // Resolving the names finds the files `include!` names that were not loaded. With those
    // in, there may be more.
    while WatchedFile::include_sources(&mut files, &mut tree, &mut db) {
        resolved = resolve_crates(&db, &args)?;
    }
    eprintln!("resolve: {resolved} crates in {:.1?}", start.elapsed());
    memory::checkpoint("resolve");
    // What the other checkouts don't share with this one, before a request waits for it.
    for also in &args.also {
        let start = Instant::now();
        let Parts { root: checkout, base, members, debug } = checkouts.parts(also)?;
        let of_it = Args { checkout, base, members, debug, ..args.clone() };
        let resolved = resolve_crates(&db, &of_it)?;
        eprintln!("resolve {}: {resolved} crates in {:.1?}", also.display(), start.elapsed());
    }
    if !args.also.is_empty() {
        report_sharing(&db);
    }

    anyhow::ensure!(args.patch.is_none() || args.isolate.is_none(), "--patch with --isolate");
    anyhow::ensure!(
        args.splice.is_none() || (args.patch.is_some() && args.link_cmd.is_some()),
        "--binary without --patch and --link-cmd"
    );
    let report_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !PROBING.get() && !INTERRUPTING.load(Ordering::SeqCst) {
            report_panic(info);
        }
    }));
    let mut known = Rejections::default();
    known.patching = args.patch.is_some();
    let listed = Arc::new(Listed::default());
    let mut outlines = outline::Outlines::default();
    let table = table.join().map_err(|_| anyhow::anyhow!("loading the symbols panicked"))??;
    let shared = shared.join().map_err(|_| anyhow::anyhow!("fetching the store panicked"))?;
    if let Some(asked) = doc {
        let krate = *crates_named(&db, &args.krate, &args)?.first().context("no such crate")?;
        let built = build(&db, &table, &args, &mut known, &listed)?;
        eprint!("doctests: ");
        report(&built);
        doctest::document(&mut db, krate);
        let code = doctest::run(&db, asked, krate, &args.out, emits.triple())?;
        std::process::exit(code);
    }
    if let Some(stale) = stale_copies(&db, &table, &args) {
        anyhow::bail!(stale);
    }
    let mut cold = build(&db, &table, &args, &mut known, &listed)?;
    // An `include!` in a body is first seen when the body is compiled.
    if WatchedFile::include_sources(&mut files, &mut tree, &mut db) {
        known.touch(&db, None);
        cold = build(&db, &table, &args, &mut known, &listed)?;
    }
    if let Some(unset) = &cold.unset {
        bail!("{unset}");
    }
    eprint!("cold: ");
    report(&cold);
    memory::report(&mut db, "build")?;
    report_stats(&known.stats);
    link(
        args.link_cmd.as_deref(),
        args.splice.as_deref(),
        args.patch.as_deref(),
        &args.out,
        false,
    )?;
    // What this build added goes to the other machines while the daemon waits for edits; a
    // build that is not a daemon's has nothing to wait for but this.
    match shared {
        Some(shared) if args.socket.is_none() && !args.watch => shared.push(),
        Some(shared) => shared.keep(),
        None => {}
    }
    hir::attach_db(&db, || known.out_of_date(&db));
    // What rustc compiled, by where in the checkout it is: the files as this build read them.
    let as_compiled: FxHashMap<PathBuf, triomphe::Arc<str>> = files
        .iter()
        .filter_map(|it| {
            let Watched::Source(id) = it.file else { return None };
            let inside = it.path.strip_prefix(&args.checkout).ok()?;
            Some((inside.to_owned(), db.file_text(id).text(&db).clone()))
        })
        .collect();
    let listener = match &args.socket {
        Some(path) => match socket::Served::bind(path) {
            Ok(served) => Some(served),
            Err(socket::NotServed::Taken) => {
                bail!("another daemon serves {}", path.display())
            }
            Err(socket::NotServed::Failed(error)) => {
                return Err(error).with_context(|| format!("listening on {}", path.display()));
            }
        },
        None if args.watch => None,
        None => return Ok(()),
    };

    // The first change after a build of everything waits for what that build left behind.
    // Paid here, the first edit does not.
    {
        use ide_db::base_db::salsa::{Database, Durability};
        db.synthetic_write(Durability::LOW);
    }
    memory::report(&mut db, "warm-up")?;
    eprintln!("watching {} files", files.len());
    ide_db::executions::take(&db);
    // Requests wait here while a build is under way, and those that have come by the time it
    // is done are built together.
    let (sender, events) = std::sync::mpsc::channel();
    let serving = listener.is_some();
    if let Some(served) = listener {
        let sender = sender.clone();
        // Each connection is read by a thread of its own: one that sends nothing, or
        // slowly, keeps no other from being heard.
        std::thread::spawn(move || {
            for (stream, open) in served.incoming() {
                let sender = sender.clone();
                std::thread::spawn(move || {
                    let _open = open;
                    if let Some(request) = requested(stream) {
                        // The daemon is ending when nothing takes requests any more.
                        let _ = sender.send(Event::Asked(Box::new(request)));
                    }
                });
            }
        });
    }
    // The first is the program asked for when a request names none.
    let (wrote, whole) = (Some(args.out.clone()), Some(args.out.clone()));
    let tests = Some(hir::attach_db(&db, || Tests::of(&db, &listed, &args)));
    let mut programs = vec![Program {
        tests,
        args,
        table: Arc::new(table),
        known,
        unbuilt: Unbuilt::default(),
        wrote,
        whole,
        interrupted: 0,
        moved: false,
    }];
    let overlay = match std::env::var("RUSTD_OVERLAY").as_deref() {
        Ok("0") => Overlay::Never,
        Ok(_) => Overlay::Always,
        Err(_) => Overlay::Quick,
    };
    let mut builds =
        Builds { events, running: FxHashMap::default(), waiting: Default::default(), stuck: false };
    // How many times running each checkout that is read as a loaded one has been long to turn
    // to or from.
    let mut slow: FxHashMap<PathBuf, u32> = FxHashMap::default();
    // The ones that are loaded instead from the next time on.
    let mut given_up: FxHashSet<PathBuf> = FxHashSet::default();
    let slow_switch = std::env::var("RUSTD_SLOW_SWITCH_MS")
        .ok()
        .and_then(|it| it.parse().ok())
        .map_or(SLOW_SWITCH, Duration::from_millis);
    // The links under way that no build waits for, by the object each is of.
    let mut links: FxHashMap<PathBuf, std::thread::JoinHandle<()>> = FxHashMap::default();
    // What the files were like when each object was last built and linked.
    let as_built: Arc<std::sync::Mutex<FxHashMap<PathBuf, Settled>>> = Arc::default();
    // The checkout each loaded one is read from, when that is not itself.
    let mut viewed: FxHashMap<PathBuf, PathBuf> = FxHashMap::default();
    // The crates whose variables name another checkout than the one they were loaded in.
    let mut placed = Placed::default();
    loop {
        if WatchedFile::unwatched(&files, &db) || tree.includes_more(&db) {
            builds.settle(&mut db, &mut programs)?;
        }
        if WatchedFile::watch_included(&mut files, &mut db)
            | WatchedFile::include_sources(&mut files, &mut tree, &mut db)
        {
            programs.iter_mut().for_each(|it| it.known.touch(&db, None));
        }
        // The program each client asks for.
        let mut clients: Vec<(usize, Asked)> = Vec::new();
        // Where each of those programs is to be written.
        let mut outputs: FxHashMap<usize, Outputs> = FxHashMap::default();
        // The checkout each loaded one has to be read from for them.
        let mut looked: FxHashMap<PathBuf, PathBuf> = FxHashMap::default();
        match serving {
            true => {
                let mut later = Vec::new();
                if builds.stuck || builds.waiting.is_empty() {
                    let event = builds.events.recv()?;
                    builds.take(event, &mut programs)?;
                }
                loop {
                    // Asked again after each: a checkout that joins takes long enough for the
                    // requests sent with its own to arrive.
                    while let Ok(event) = builds.events.try_recv() {
                        builds.take(event, &mut programs)?;
                    }
                    let Some((request, target, stream)) = builds.waiting.pop_front() else {
                        break;
                    };
                    if request == Request::Select {
                        let picked = picked(&db, &programs, target.as_ref(), &stream, &files);
                        let said = serde_json::to_string(&picked).unwrap_or_default();
                        let _ = writeln!(&stream, "ok {said}");
                        continue;
                    }
                    let asked = target.clone();
                    let mut target = target;
                    // A checkout that is not what it was loaded from is loaded again before
                    // anything is built of it. The builds under way and the ones already
                    // taken for this turn are of the checkout as it was.
                    let project = target.as_ref().and_then(|it| it.project.as_ref());
                    let project = project.unwrap_or(&programs[0].args.project);
                    match checkouts.stale(project, &stream.env) {
                        Ok(stale) if stale.is_empty() => {}
                        Ok(_) if !clients.is_empty() => {
                            later.push((request, asked, stream));
                            continue;
                        }
                        Ok(stale) => {
                            builds.settle(&mut db, &mut programs)?;
                            let start = Instant::now();
                            let before = CratesAsLoaded::of(&db);
                            let reloaded =
                                checkouts.reload(&stale, &stream.env, &mut db, tree.vfs());
                            watch_loaded(&mut db, &mut tree, &mut files);
                            let new = before.changed(&db);
                            eprintln!(
                                "reload: {} crates are others in {:.1?}",
                                new.len(),
                                start.elapsed()
                            );
                            let mut started = Ok(());
                            for program in &mut programs {
                                let parts = checkouts.parts(&program.args.project)?;
                                let linked = target
                                    .as_ref()
                                    .filter(|it| it.is(&program.args))
                                    .filter(|it| !it.symbol_files.is_empty());
                                let same = parts.members == program.args.members
                                    && parts.debug == program.args.debug;
                                if same && (linked.is_none() || new.is_empty()) {
                                    program.known.touch(&db, Some(&new));
                                    program.unbuilt.stale |= !new.is_empty();
                                    continue;
                                }
                                let again = program.start_over(&db, parts, linked, &new, &files);
                                started = started.and(again);
                            }
                            listed.touch(&db, Some(&new));
                            as_built.lock().unwrap().clear();
                            if let Err(error) = reloaded.and(started) {
                                let _ = writeln!(&stream, "error: {error:#}");
                                continue;
                            }
                        }
                        Err(error) => {
                            let _ = writeln!(&stream, "error: {error:#}");
                            continue;
                        }
                    }
                    // One build is for one process: its variables are what `env!` reads.
                    if process::differs(&db, &stream.env) {
                        if !clients.is_empty() {
                            later.push((request, asked, stream));
                            continue;
                        }
                        builds.settle(&mut db, &mut programs)?;
                        let reading = process::turn(&mut db, stream.env.clone());
                        if !reading.is_empty() {
                            for program in &mut programs {
                                program.known.touch(&db, Some(&reading));
                                program.unbuilt.stale = true;
                            }
                            listed.touch(&db, Some(&reading));
                            as_built.lock().unwrap().clear();
                        }
                    }
                    let view = match target.as_ref().and_then(|it| it.project.as_ref()) {
                        Some(project) => {
                            // What a checkout reads is known once a build of it is over.
                            let pending = WatchedFile::unwatched(&files, &db);
                            let built: Vec<&Path> = programs
                                .iter()
                                .enumerate()
                                .filter(|(at, it)| {
                                    it.whole.is_some() && !builds.running.contains_key(at)
                                })
                                .map(|(_, it)| &*it.args.checkout)
                                .collect();
                            let quick = |base: &Path, over: &Path| {
                                !pending && built.contains(&base) && !given_up.contains(over)
                            };
                            checkouts.view_of(project, overlay, &quick)
                        }
                        None => {
                            let base = programs[0].args.checkout.clone();
                            let project = programs[0].args.project.clone();
                            Some(View { over: base.clone(), base, project })
                        }
                    };
                    // A checkout read as another that is as it was built takes no turn.
                    if let (Some(view), Some(target)) = (&view, &target) {
                        let unchanged = links.get(&target.out).is_none_or(|it| it.is_finished())
                            && as_built
                                .lock()
                                .unwrap()
                                .get(&target.out)
                                .is_some_and(|it| it.still(&files, &db, view, target, request));
                        if unchanged {
                            let _ = writeln!(&stream, "ok unchanged");
                            continue;
                        }
                    }
                    // One build reads a checkout from one place, and one that has packages of
                    // another reads that one as it is.
                    let shares = view
                        .as_ref()
                        .and_then(|it| checkouts.shares_with(&it.base))
                        .map(Path::to_owned);
                    if let Some(view) = &view
                        && (looked.get(&view.base).is_some_and(|over| *over != view.over)
                            || shares.as_ref().is_some_and(|base| {
                                looked.get(base).is_some_and(|over| over != base)
                            }))
                    {
                        later.push((request, target, stream));
                        continue;
                    }
                    if let (Some(view), Some(target)) = (&view, &mut target)
                        && target.project.is_some()
                    {
                        target.project = Some(view.project.clone());
                    }
                    let shared = view.as_ref().is_some_and(|view| {
                        view.base != view.over || viewed.contains_key(&view.base)
                    });
                    let known = match (&target, &view) {
                        (Some(target), _) if programs.iter().any(|it| target.is(&it.args)) => {
                            programs.iter().position(|it| target.is(&it.args))
                        }
                        // The checkouts read as one share its programs.
                        (Some(target), Some(view)) if shared => programs.iter().position(|it| {
                            it.args.krate == target.krate && it.args.checkout == view.base
                        }),
                        (Some(_), _) => None,
                        (None, _) => Some(0),
                    };
                    let wanted = match &target {
                        Some(target) => Outputs {
                            out: target.out.clone(),
                            patch: target.patch.clone(),
                            link_cmd: target.link_cmd.clone(),
                            splice: target.splice.clone(),
                        },
                        None => Outputs::of(&programs[0].args),
                    };
                    if let Some(at) = known {
                        if builds.running.contains_key(&at)
                            || outputs.get(&at).is_some_and(|it| *it != wanted)
                        {
                            later.push((request, asked, stream));
                            continue;
                        }
                        outputs.insert(at, wanted);
                        if let Some(view) = view {
                            looked.insert(view.base, view.over);
                        }
                        if let Some(base) = shares {
                            looked.insert(base.clone(), base);
                        }
                        clients.push((at, (request, asked, stream)));
                        continue;
                    }
                    let Some(target) = target else { continue };
                    // A checkout the daemon has not seen joins the ones it has.
                    let joined = match &target.project {
                        Some(project) if view.is_none() => {
                            builds.settle(&mut db, &mut programs)?;
                            let start = Instant::now();
                            let joined =
                                join(project, &mut checkouts, &mut db, &mut tree, &mut files);
                            eprintln!("join {}: {:.1?}", project.display(), start.elapsed());
                            joined
                        }
                        _ => Ok(()),
                    };
                    if let Some(view) = view {
                        looked.insert(view.base, view.over);
                    }
                    let shares = target
                        .project
                        .as_ref()
                        .and_then(|it| checkouts.parts(it).ok())
                        .and_then(|it| checkouts.shares_with(&it.root).map(Path::to_owned));
                    if let Some(base) = shares {
                        // Whoever else reads it in this build reads it so.
                        if looked.get(&base).is_some_and(|over| *over != base) {
                            later.push((request, asked, stream));
                            continue;
                        }
                        looked.insert(base.clone(), base);
                    }
                    let mut known = Rejections::default();
                    known.patching = target.patch.is_some();
                    let loaded = joined
                        .and_then(|()| target.args(&programs[0].args, &checkouts))
                        .and_then(|args| {
                            // The symbols of a checkout are those of the one it shares
                            // packages with, which rustc built.
                            let shared = programs.iter().find(|it| {
                                it.args.symbol_files == args.symbol_files
                                    && (it.args.checkout == args.checkout
                                        || Some(&it.args.checkout) == args.base.as_ref())
                            });
                            let table = match shared {
                                Some(program) => program.table.clone(),
                                None => {
                                    Arc::new(SymbolTable::load(&args.symbol_files, &args.root()?)?)
                                }
                            };
                            Ok((args, table))
                        });
                    match loaded {
                        Ok((args, table)) => {
                            // Never built: every file is as good as edited, but those were
                            // checked when the programs before it were built.
                            let unbuilt = Unbuilt {
                                edited: programs[0].unbuilt.edited.clone(),
                                stale: true,
                                unlinked: false,
                                named_anew: true,
                            };
                            let mut program = Program {
                                tests: None,
                                args,
                                table,
                                known,
                                unbuilt,
                                wrote: None,
                                whole: None,
                                interrupted: 0,
                                moved: false,
                            };
                            if let Some(built) =
                                programs.iter().find(|it| it.same_as(&db, &program))
                            {
                                program.known = built.known.clone();
                                program.unbuilt.stale = false;
                                program.whole = built.whole.clone();
                            }
                            let first = program.args.symbol_files == programs[0].args.symbol_files;
                            program.distrust_rustc(&db, &files, first.then_some(&as_compiled));
                            clients.push((programs.len(), (request, asked, stream)));
                            outputs.insert(programs.len(), wanted);
                            programs.push(program);
                        }
                        // A request for what can't be built is the client's mistake.
                        Err(e) => {
                            let _ = writeln!(&stream, "error: {e:#}");
                        }
                    }
                }
                builds.waiting.extend(later);
                builds.stuck = clients.is_empty();
                if builds.stuck {
                    continue;
                }
                // A package that was the same in two checkouts may have been edited in one.
                let start = Instant::now();
                let differences = checkouts.differences()?;
                if !differences.is_empty() {
                    builds.settle(&mut db, &mut programs)?;
                    checkouts.share(&mut db, tree.vfs(), differences)?;
                    watch_loaded(&mut db, &mut tree, &mut files);
                    for program in &mut programs {
                        let Parts { base, members, .. } = checkouts.parts(&program.args.project)?;
                        program.moved |= program.args.members != members;
                        (program.args.base, program.args.members) = (base, members);
                        program.unbuilt.stale = true;
                        program.known.touch(&db, None);
                    }
                    listed.touch(&db, None);
                    eprintln!("split again: {:.1?}", start.elapsed());
                }
            }
            false => std::thread::sleep(Duration::from_millis(5)),
        }
        // The checkouts that are read from somewhere else than for the last build. Their files
        // have the times of another checkout, so they are compared by what is in them.
        let mut switched = Vec::new();
        // The checkouts this build stops or starts reading a loaded one from.
        let mut turned = Vec::new();
        // The crates that are told another place than for the last build.
        let mut moved = Vec::new();
        for (base, over) in looked {
            if *viewed.get(&base).unwrap_or(&base) != over {
                eprintln!("view: {} as {}", over.display(), base.display());
                builds.settle(&mut db, &mut programs)?;
                turned.extend(viewed.get(&base).map(|it| (base.clone(), it.clone())));
                turned.extend((base != over).then(|| (base.clone(), over.clone())));
                db.set_view(&base, &over, built_in(&db, &base));
                moved.extend(place(&mut db, &base, &over, &mut placed));
                tree.relist(&base);
                if base == over {
                    viewed.remove(&base);
                } else {
                    viewed.insert(base.clone(), over);
                }
                switched.push(base);
            }
        }
        // A checkout read as another is read so by every build, so those are waited for.
        let background = serving && overlay != Overlay::Always && viewed.is_empty();
        // Checkouts read as one are built in turn, so their links don't take the turn.
        let detached = serving && !viewed.is_empty();
        for (&at, wanted) in &outputs {
            let program = &mut programs[at];
            program.unbuilt.stale |= program.wrote.as_ref() != Some(&wanted.out);
            // The files a link reads and writes are not written again under it.
            if let Some(link) = links.remove(&wanted.out) {
                _ = link.join();
            }
            as_built.lock().unwrap().remove(&wanted.out);
        }
        let mut noticed = None;
        // Keep scanning until a pass finds nothing new, so one save of several files is one build.
        // Files are read after that: a timestamp can be that of a write still under way, and
        // the rest of that write may not change it again. Whoever asks for a build has finished
        // writing, so one pass is enough then.
        let mut touched = FxHashSet::default();
        // Source files that are no longer there.
        let mut gone = FxHashSet::default();
        loop {
            if tree.relisted(&db) {
                builds.settle(&mut db, &mut programs)?;
            }
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
                let on_disk = db.on_disk(&file.path);
                let Ok(now) = std::fs::metadata(&on_disk).and_then(|m| m.modified()) else {
                    if matches!(file.file, Watched::Source(_)) && gone.insert(index) {
                        touched.remove(&index);
                        noticed.get_or_insert((Instant::now(), Duration::ZERO));
                    }
                    continue;
                };
                gone.remove(&index);
                if switched.iter().any(|base| file.path.starts_with(base)) {
                    file.mtime = now;
                    let same = match file.file {
                        Watched::Source(id) => source_on_disk(&on_disk)
                            .is_ok_and(|text| *text == **db.file_text(id).text(&db)),
                        Watched::Included(ref path) => db.included_is_read(path),
                    };
                    if !same {
                        touched.insert(index);
                        noticed.get_or_insert((Instant::now(), Duration::ZERO));
                    }
                } else if now != file.mtime {
                    file.mtime = now;
                    touched.insert(index);
                    noticed.get_or_insert((Instant::now(), now.elapsed().unwrap_or_default()));
                }
            }
            switched.clear();
            if touched.len() + gone.len() == before || !clients.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Without clients, the first program is built whenever a file changes.
        let building: FxHashSet<usize> = match serving {
            true => clients.iter().map(|it| it.0).collect(),
            false => FxHashSet::from_iter([0]),
        };
        for (at, (request, ..)) in &clients {
            let unbuilt = &mut programs[*at].unbuilt;
            unbuilt.stale |= unbuilt.unlinked && *request != Request::Patch;
        }
        let stale = building.iter().any(|&at| programs[at].unbuilt.stale) || !moved.is_empty();
        let (noticed, age) = match noticed {
            Some(noticed) => noticed,
            None if stale => (Instant::now(), Duration::ZERO),
            None => {
                for (_, (_, _, client)) in clients {
                    let _ = writeln!(&client, "ok unchanged");
                }
                continue;
            }
        };
        if touched.len() + gone.len() != 0 {
            builds.settle(&mut db, &mut programs)?;
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
                change.change_file(id, None);
                sources.push(id);
                removed.push(id);
            }
        }
        for index in touched {
            let WatchedFile { file, path, .. } = &files[index];
            match file {
                Watched::Source(id) => {
                    outlines.before_edit(&db, &listed, *id);
                    let text = source_on_disk(&db.on_disk(path))?;
                    change.change_file(*id, Some(text));
                    sources.push(*id);
                }
                Watched::Included(path) => included.push(path.clone()),
            }
        }
        let read = read.elapsed();
        eprintln!("change: {changed} files");
        let start = Instant::now();
        // A write stops the builds under way, so nothing is written when nothing changed.
        if changed != 0 || !background {
            db.apply_change(change);
        }
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
            programs.iter_mut().for_each(|it| it.known.touch(&db, None));
            listed.touch(&db, None);
        }
        for path in included {
            db.reload_included_file(&path);
        }
        // Asked of every file before any is noted: noting one forgets the functions listed.
        for program in &mut programs {
            let unbuilt = &mut program.unbuilt;
            unbuilt.stale |= changed != 0;
            unbuilt.edited.extend(&sources);
            unbuilt.edited.sort_unstable();
            unbuilt.edited.dedup();
        }
        let paths: FxHashMap<_, _> = files
            .iter()
            .filter_map(|it| match it.file {
                Watched::Source(id) => Some((id, it.path.clone())),
                Watched::Included(_) => None,
            })
            .collect();
        let paths = Arc::new(Sources { paths });
        let sources = outlines.after_edit(&db, &listed, sources);
        for (file, bodies_only) in sources {
            if bodies_only {
                if std::env::var_os("RUSTD_STATS").is_some() {
                    eprintln!("bodies only: {file:?}");
                }
                programs.iter_mut().for_each(|it| it.known.touch_bodies(&db, file));
                continue;
            }
            let crates = ide_db::base_db::relevant_crates(&db, file);
            let crates = (!crates.is_empty()).then_some(crates);
            programs.iter_mut().for_each(|it| it.known.touch(&db, crates));
            listed.touch(&db, crates);
        }
        if !moved.is_empty() {
            programs.iter_mut().for_each(|it| {
                it.known.touch(&db, Some(&moved));
                it.unbuilt.stale = true;
            });
            listed.touch(&db, Some(&moved));
        }
        let edit = Edit { start, age, settled, read, apply: start.elapsed(), changed };
        if background {
            let mut asked: FxHashMap<usize, Vec<Asked>> = FxHashMap::default();
            for (at, client) in clients {
                asked.entry(at).or_default().push(client);
            }
            for (at, clients) in asked {
                let program = &mut programs[at];
                if !program.unbuilt.stale {
                    for (_, _, client) in clients {
                        let _ = writeln!(&client, "ok unchanged");
                    }
                    continue;
                }
                // The program is linked unless every client of it loads the patch.
                let link = clients.iter().any(|it| it.0 != Request::Patch);
                let patches = program.known.patches() && !program.moved;
                let kept = patches || program.interrupted >= INTERRUPTIONS;
                // What stands in for the program while it is built says what program it is.
                let away = Program {
                    args: program.args.clone(),
                    table: program.table.clone(),
                    known: Rejections::default(),
                    unbuilt: Unbuilt::default(),
                    wrote: None,
                    whole: None,
                    interrupted: program.interrupted,
                    moved: program.moved,
                    tests: None,
                };
                let mut program = Box::new(std::mem::replace(program, away));
                builds.running.insert(at, Running { clients, kept });
                let (db, listed, paths, sender) =
                    (db.clone(), listed.clone(), paths.clone(), sender.clone());
                let wanted = outputs.remove(&at);
                std::thread::spawn(move || {
                    let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        program.sync(&db, &listed, &paths, &edit, wanted.as_ref(), link)
                    }));
                    // The write that interrupts waits for this.
                    drop(db);
                    let answer = match answer {
                        _ if INTERRUPTING.load(Ordering::SeqCst) => None,
                        Ok(answer) => Some(answer),
                        Err(_) => Some(Err(anyhow::anyhow!("a build panicked"))),
                    };
                    let _ = sender.send(Event::Built(at, program, answer));
                });
            }
            continue;
        }
        // The programs asked for are built side by side: nothing is written to the database
        // until all of them are done, so none is interrupted.
        let answers = loop {
            let answers: Vec<(usize, anyhow::Result<Result<String, String>>)> =
                std::thread::scope(|scope| {
                    let builds: Vec<_> = programs
                        .iter_mut()
                        .enumerate()
                        .filter(|(at, program)| building.contains(at) && program.unbuilt.stale)
                        .map(|(at, program)| {
                            // The program is linked unless every client of it loads the patch.
                            let link = !clients.iter().any(|it| it.0 == at)
                                || clients.iter().any(|it| it.0 == at && it.1.0 != Request::Patch);
                            let link = link && !detached;
                            let (db, listed, paths, edit) = (db.clone(), &*listed, &*paths, &edit);
                            let wanted = outputs.get(&at);
                            let build = scope.spawn(move || {
                                program.sync(&db, listed, paths, edit, wanted, link)
                            });
                            (at, build)
                        })
                        .collect();
                    builds
                        .into_iter()
                        .map(|(at, build)| {
                            let answer = build
                                .join()
                                .unwrap_or_else(|_| Err(anyhow::anyhow!("a build panicked")));
                            (at, answer)
                        })
                        .collect()
                });
            // An `env!` the builds were the first to see was told the place of the loaded
            // checkout.
            let late: Vec<_> = viewed
                .iter()
                .flat_map(|(base, over)| place(&mut db, base, over, &mut placed))
                .collect();
            if late.is_empty() {
                break answers;
            }
            for program in &mut programs {
                program.known.touch(&db, Some(&late));
                program.unbuilt.stale = true;
            }
            listed.touch(&db, Some(&late));
        };
        if std::env::var_os("RUSTD_TRACE").is_some() {
            let ran: Vec<String> = ide_db::executions::take(&db)
                .iter()
                .take(400)
                .map(|(query, count)| format!("{query}={count}"))
                .collect();
            eprintln!("ran: {}", ran.join(" "));
            eprintln!("order: {}", ide_db::executions::order(&db).join(" > ").replace(' ', ""));
        }
        // Turning from one that is given up on is the last time it is paid for.
        if !turned.iter().any(|it| given_up.contains(&it.1)) {
            let long = edit.start.elapsed() > slow_switch;
            for (_, over) in &turned {
                let times = slow.entry(over.clone()).or_default();
                *times = if long { *times + 1 } else { 0 };
            }
            if turned.iter().any(|it| slow[&it.1] >= SLOW_SWITCHES) {
                // Turning between two is as long for the one as for the other: the one that
                // is further from what is loaded is what takes long.
                let far = turned.iter().max_by_key(|(base, over)| {
                    let differs = |file: &&WatchedFile| {
                        let Ok(inside) = file.path.strip_prefix(base) else { return false };
                        std::fs::read(&file.path).ok() != std::fs::read(over.join(inside)).ok()
                    };
                    files.iter().filter(differs).count()
                });
                given_up.extend(far.map(|it| it.1.clone()));
                slow.clear();
            }
        }
        let mut replies = FxHashMap::default();
        for (at, answer) in answers {
            replies.insert(at, answer?);
        }
        let mut linking: FxHashMap<usize, Vec<Asked>> = FxHashMap::default();
        for (at, asked) in clients {
            let mut client = &asked.2;
            let _ = match replies.get(&at) {
                Some(Ok(_)) if detached => {
                    linking.entry(at).or_default().push(asked);
                    continue;
                }
                Some(Ok(built)) => writeln!(client, "ok {built}"),
                Some(Err(why)) => writeln!(client, "error: {}", why.replace('\n', " ")),
                None => writeln!(client, "ok unchanged"),
            };
        }
        for (at, clients) in linking {
            let Some(Ok(built)) = replies.remove(&at) else { continue };
            let Outputs { out, link_cmd, patch, splice } =
                outputs.remove(&at).unwrap_or_else(|| Outputs::of(&programs[at].args));
            // The answer names a patch when the whole object is as it was.
            let patched = built.starts_with("patch=");
            let spliced = patch.clone();
            let link_cmd = link_cmd.filter(|_| clients.iter().any(|it| it.0 != Request::Patch));
            let sender = sender.clone();
            let base = &programs[at].args.checkout;
            let over = viewed.get(base).unwrap_or(base);
            let from = Settled::of(&files, &db, base, over, patch, link_cmd.clone());
            let (as_built, object) = (as_built.clone(), out.clone());
            let linked = std::thread::spawn(move || {
                let failed = link(
                    link_cmd.as_deref(),
                    splice.as_deref(),
                    spliced.as_deref(),
                    &object,
                    patched,
                )
                .err();
                if failed.is_none() {
                    as_built.lock().unwrap().insert(object, from);
                }
                for (_, _, client) in clients {
                    let _ = match &failed {
                        None => writeln!(&client, "ok {built}"),
                        Some(why) => writeln!(&client, "error: {why}"),
                    };
                }
                if failed.is_some() {
                    let _ = sender.send(Event::Unlinked(at));
                }
            });
            links.insert(out, linked);
        }
    }
}

/// What an object of a checkout read as another was built from, to tell whether it still is.
struct Settled {
    over: PathBuf,
    patch: Option<PathBuf>,
    /// `None` when it was not linked.
    link_cmd: Option<String>,
    /// The checkout's own files and their directories, with when each was written. Another
    /// checkout may have other files, and a file that is new changes none of those there are.
    own: Vec<(PathBuf, Option<SystemTime>)>,
    /// When each file outside the checkout was written.
    others: u64,
}

impl Settled {
    /// To be asked once the build is over: the files are as old as they were when the build
    /// started to read them.
    fn of(
        files: &[WatchedFile],
        db: &RootDatabase,
        base: &Path,
        over: &Path,
        patch: Option<PathBuf>,
        link_cmd: Option<String>,
    ) -> Settled {
        let mut own = Vec::new();
        let mut directories = FxHashSet::default();
        for file in files.iter().filter(|it| it.path.starts_with(base)) {
            let path = db.on_disk(&file.path);
            if let Some(directory) = path.parent()
                && path.starts_with(over)
                && directories.insert(directory.to_owned())
            {
                own.push((directory.to_owned(), written(directory)));
            }
            own.push((path, Some(file.mtime)));
        }
        let others = Settled::others(files, base, |file| Some(file.mtime));
        Settled { over: over.to_owned(), patch, link_cmd, own, others }
    }

    fn others(
        files: &[WatchedFile],
        base: &Path,
        written: impl Fn(&WatchedFile) -> Option<SystemTime>,
    ) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = rustc_hash::FxHasher::default();
        for file in files.iter().filter(|it| !it.path.starts_with(base)) {
            (&file.path, written(file)).hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Whether a build for `request` would find the files as they were and write what is
    /// there.
    fn still(
        &self,
        files: &[WatchedFile],
        db: &RootDatabase,
        view: &View,
        target: &Target,
        request: Request,
    ) -> bool {
        let linked = request == Request::Patch || self.link_cmd.is_some();
        // A file outside every checkout is read from where it is.
        let outside = |file: &WatchedFile| written(&db.on_disk(&file.path));
        self.over == view.over
            && self.patch == target.patch
            && linked
            && (self.link_cmd.is_none() || self.link_cmd == target.link_cmd)
            && target.out.exists()
            && self.patch.as_ref().is_none_or(|it| it.exists())
            && self.own.iter().all(|(path, at)| written(path) == *at)
            && self.others == Settled::others(files, &view.base, outside)
    }
}

fn written(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|it| it.modified()).ok()
}

/// A change to the files, and how long each step up to the builds took.
#[derive(Clone, Copy)]
struct Edit {
    /// When the change was applied.
    start: Instant,
    age: Duration,
    settled: Duration,
    read: Duration,
    apply: Duration,
    /// How many files changed.
    changed: usize,
}

impl Program {
    /// Notes the crates rustc did not compile from the files this program is of, when it joins
    /// linked with what rustc built: of the first checkout, whose files as the daemon read them
    /// are `as_compiled`, or of its own checkout as it was when rustc built it.
    fn distrust_rustc(
        &mut self,
        db: &RootDatabase,
        files: &[WatchedFile],
        as_compiled: Option<&FxHashMap<PathBuf, triomphe::Arc<str>>>,
    ) {
        let built = self
            .args
            .symbol_files
            .iter()
            .filter(|it| !it.components().any(|it| it.as_os_str() == "rustlib"))
            .filter_map(|it| std::fs::metadata(it).ok()?.modified().ok())
            .max();
        let compiled_from = |file: &WatchedFile, id: vfs::FileId| {
            let inside = file.path.strip_prefix(&self.args.checkout).ok()?;
            Some(match as_compiled {
                Some(as_compiled) => {
                    let text = db.file_text(id).text(db);
                    as_compiled.get(inside).is_some_and(|was| **was == **text)
                }
                None => file.mtime <= built?,
            })
        };
        let other: Vec<_> = files
            .iter()
            .filter_map(|it| {
                let Watched::Source(id) = it.file else { return None };
                let fresh = compiled_from(it, id)?;
                (!fresh).then(|| ide_db::base_db::relevant_crates(db, id).to_vec())
            })
            .flatten()
            .collect();
        self.known.distrust(db, &other);
    }

    /// Has the program be one of its checkout as it was loaded again: of the crates and the
    /// debug information of `parts`, and never built. It is linked as `linked` says, when
    /// that names what rustc built of the checkout since, and else with what it was linked
    /// with, where the crates in `new` are not the ones rustc compiled.
    fn start_over(
        &mut self,
        db: &RootDatabase,
        parts: Parts,
        linked: Option<&Target>,
        new: &[ide_db::base_db::Crate],
        files: &[WatchedFile],
    ) -> anyhow::Result<()> {
        let Parts { root, base, members, debug } = parts;
        (self.args.checkout, self.args.base) = (root, base);
        (self.args.members, self.args.debug) = (members, debug);
        let patching = self.known.patching;
        self.known = Rejections::default();
        self.known.patching = patching;
        (self.whole, self.moved) = (None, true);
        self.unbuilt.stale = true;
        match linked {
            Some(linked) => {
                self.args.symbol_files = linked.symbol_files.clone();
                if linked.link_cmd.is_some() {
                    self.args.link_cmd = linked.link_cmd.clone();
                }
                self.table =
                    Arc::new(SymbolTable::load(&self.args.symbol_files, &self.args.root()?)?);
                self.distrust_rustc(db, files, None);
                self.unbuilt.named_anew = true;
                self.args.widened.clear();
            }
            None => self.known.distrust(db, new),
        }
        Ok(())
    }

    /// Takes over from rustc the crates that an edit since the last build is to, of those the
    /// program uses and the daemon was not asked to build, and the crates between them and
    /// the ones it was: rustc's copies of them are from before the edit. The result is why
    /// one can't be taken over, when one can't.
    fn widen(&mut self, db: &RootDatabase) -> Option<String> {
        let built: Vec<ide_db::base_db::Crate> = self
            .args
            .crates()
            .filter_map(|name| crates_named(db, name, &self.args).ok())
            .flatten()
            .map(Into::into)
            .collect();
        let used: FxHashSet<_> = built.iter().flat_map(|it| it.transitive_deps(db)).collect();
        let edited: Vec<_> = self
            .unbuilt
            .edited
            .iter()
            .filter_map(|&file| outline::left_to_rustc(db, &built, &used, file))
            .collect();
        if edited.is_empty() {
            return None;
        }
        let mut stale: Vec<_> = edited.iter().flat_map(|it| it.transitive_rev_deps(db)).collect();
        stale.retain(|it| used.contains(it) && !built.contains(it));
        stale.sort_unstable();
        stale.dedup();
        for &krate in &stale {
            let Some(root) = crate_root_file(db, krate.into()) else {
                let name = krate.extra_data(db).display_name.as_ref().map(|it| it.to_string());
                return Some(format!(
                    "an edit is to `{}`, which the program uses and the daemon was not asked to \
                     build: its root file has no path to build it by",
                    name.unwrap_or_default()
                ));
            };
            let root = root.display().to_string();
            eprintln!("widened: {root} is built here from now on, rustc's copy is of before");
            self.args.widened.push(root);
        }
        self.known.distrust(db, &edited);
        self.known.forget_base();
        (self.whole, self.moved) = (None, true);
        None
    }

    /// Reads what rustc built again when it has built since, in a program whose crates
    /// differ in `-C overflow-checks`: which crate holds a copy of a function decides
    /// whether a call of it checks, and what was compiled by the copies of before is not
    /// kept.
    fn read_copies_again(&mut self, db: &RootDatabase) -> anyhow::Result<()> {
        let args = &self.args;
        let built: Vec<Crate> =
            args.crates().filter_map(|it| crates_named(db, it, args).ok()).flatten().collect();
        if !self.table.built_since_read() || !checks_differ(db, &built) {
            return Ok(());
        }
        self.table = Arc::new(SymbolTable::load(&args.symbol_files, &args.root()?)?);
        let patching = self.known.patching;
        self.known = Rejections::default();
        self.known.patching = patching;
        (self.whole, self.moved) = (None, true);
        self.unbuilt.stale = true;
        Ok(())
    }

    /// Whether the last build of this program is one of `other`, which has had none: the two
    /// are of the same crates, as checkouts that share them have it. What `other` is then
    /// asked for is a patch of that build.
    fn same_as(&self, db: &RootDatabase, other: &Program) -> bool {
        let (ours, theirs) = (&self.args, &other.args);
        let crates = |args: &Args| {
            let names = args.crates();
            names.map(|it| crates_named(db, it, args).ok()).collect::<Option<Vec<_>>>()
        };
        !self.unbuilt.stale
            && self.whole.is_some()
            && self.known.patches()
            && self.known.patching == other.known.patching
            && Arc::ptr_eq(&self.table, &other.table)
            && (ours.cfg_test, &ours.selection) == (theirs.cfg_test, &theirs.selection)
            && crates(ours).is_some_and(|it| Some(it) == crates(theirs))
    }

    /// Brings the program up to date with the database. The result is what to tell its
    /// clients: what they have to load, or why the source can't be built.
    fn sync(
        &mut self,
        db: &RootDatabase,
        listed: &Listed,
        paths: &Sources,
        edit: &Edit,
        wanted: Option<&Outputs>,
        link_it: bool,
    ) -> anyhow::Result<Result<String, String>> {
        // The crates left to rustc are as edited when a client has named what it built since.
        let named_anew = std::mem::take(&mut self.unbuilt.named_anew);
        let unnamed = if named_anew { None } else { self.widen(db) };
        self.read_copies_again(db)?;
        let Program { args, table, known, unbuilt, wrote, whole, tests, .. } = self;
        let Outputs { out, patch, link_cmd, splice } =
            wanted.cloned().unwrap_or_else(|| Outputs::of(args));
        let args = &Args { out, patch, link_cmd, splice, ..args.clone() };
        // A patch is for the object with all of the program, which this build may be the
        // first to be asked for at this place.
        if let Some(whole) = whole.as_ref().filter(|it| **it != args.out) {
            if !same_file::is_same_file(whole, &args.out).unwrap_or(false) {
                _ = std::fs::remove_file(&args.out);
                if std::fs::hard_link(whole, &args.out).is_err() {
                    std::fs::copy(whole, &args.out)?;
                }
            }
        }
        let &Edit { start, age, settled, read, apply, changed } = edit;
        // Another checkout's files are not this program's to be refused for.
        let mut edited = unbuilt.edited.clone();
        edited.retain(|file| paths.paths.get(file).is_none_or(|it| args.owns(db, *file, it)));
        // Neither is what no crate that is built reads: an example, or a test.
        edited.retain(|&file| listed.sees(db, file));
        // The check of the edited files does not wait for the build: it reads other things.
        let (warm, broken) = std::thread::scope(|scope| {
            let check = {
                let (db, edited) = (db.clone(), &edited);
                scope.spawn(move || broken_in(&db, edited))
            };
            let warm = build(db, table, args, known, listed);
            (warm, check.join().ok().flatten())
        });
        let warm = warm?;
        if known.stats.patched.is_none() {
            *whole = Some(args.out.clone());
        }
        *wrote = Some(args.out.clone());
        let built = start.elapsed();
        eprint!("edit (apply {apply:.1?}): ");
        report(&warm);
        report_stats(&known.stats);
        let reported = start.elapsed();
        let out_of_date = hir::attach_db(db, || known.out_of_date(db));
        let mut failed = unnamed
            .or_else(|| refusal(db, &edited, paths, out_of_date))
            .or(broken)
            .or_else(|| warm.unset.clone());
        failed = failed.or_else(|| stale_copies(db, table, args));
        let tests_now = hir::attach_db(db, || Tests::of(db, listed, args));
        match tests {
            Some(first) => failed = failed.or_else(|| first.changed(&tests_now)),
            // The first build of the program is of the source rustc built it from.
            None if failed.is_none() => *tests = Some(tests_now),
            None => {}
        }
        if failed.is_none() || known.stats.patched.is_none() {
            let patched = known.stats.patched.is_some();
            let (splice, patch) = (args.splice.as_deref(), args.patch.as_deref());
            let cmd = args.link_cmd.as_deref().filter(|_| failed.is_none() && link_it);
            let unlinked = link(cmd, splice, patch, &args.out, patched).err();
            failed = failed.or(unlinked.map(|it| it.to_string()));
        }
        if let Some(why) = &failed {
            eprintln!("refused: {why}");
        } else {
            let unlinked = args.link_cmd.is_some() && !link_it;
            *unbuilt = Unbuilt { unlinked, ..Unbuilt::default() };
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
                ("discarded", phases.discarded),
                (
                    "compile-rest",
                    compile.saturating_sub(
                        phases.compile
                            + phases.resolve
                            + phases.define
                            + phases.object
                            + phases.discarded,
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
        eprintln!("edit-to-binary: {total:.1?}");
        if let Some(why) = failed {
            return Ok(Err(why));
        }
        // What the caller has to load to be up to date: the functions that differ from the
        // first object when the edit could be a patch, and all of them when it could not.
        let (kind, object) = match (&args.patch, known.stats.patched) {
            (Some(patch), Some(_)) => ("patch", patch),
            _ => ("object", &args.out),
        };
        Ok(Ok(format!(
            "{kind}={} functions={} files={changed} ms={}",
            object.display(),
            known.stats.patched.unwrap_or(known.stats.compiled),
            total.as_millis()
        )))
    }
}

#[allow(dead_code)]
fn abs(path: &Path) -> anyhow::Result<VfsPath> {
    Ok(VfsPath::from(AbsPathBuf::assert_utf8(dunce::canonicalize(path)?)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_command_of_lto_has_no_core() {
        let ordinary = r#"cc "/t/deps/app-1.app.0.rcgu.o" "/sysroot/lib/libcore-667b96598095cbda.rlib" -o /t/app"#;
        let lto = r#"cc "/t/deps/app-1.app.0.rcgu.o" "/sysroot/lib/libcompiler_builtins-2ae2.rlib" -o /t/app"#;
        assert!(links_core(ordinary));
        assert!(links_core("cc @/t/link.args"));
        assert!(links_core("cc /sysroot/lib/libstd-44675f186029a4cd.so"));
        assert!(!links_core(lto));
    }

    #[test]
    fn the_commit_of_a_toolchain_is_the_one_its_library_has_its_sources_under() {
        let commit = "b940084d7eb6a299eb4bfeb8e34901bc051e7ac4";
        let built = format!("\0/rustc/short/a.rs\0/rustc/{commit}x\0/rustc/{commit}/library/b.rs");
        assert_eq!(commit_of_sources(built.as_bytes()).as_deref(), Some(commit));
        assert_eq!(commit_of_sources(b"/rustc/"), None);
        assert_eq!(commit_of_sources(format!("/rustc/{commit}").as_bytes()), None);
    }
}
