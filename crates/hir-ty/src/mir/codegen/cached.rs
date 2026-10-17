//! Compiled functions kept between runs of the daemon by their stable names.
//!
//! An [`Entry`] is a function of a crate that no edit reaches, instantiated with types of such
//! crates, as it was stitched into an object: its code, and what the code refers to by names
//! that mean the same in every run. A build that finds an entry for an instance, and one for
//! everything the entry calls, puts them in the object without asking the database anything
//! about the instance.
//!
//! What an entry imports from rustc is a symbol of the program it was compiled for. Another
//! program may link against a build of the crates without it, so a lookup checks that the
//! symbols are there.

use std::sync::{Arc, Mutex, OnceLock};

use cranelift_codegen::{
    binemit::{CodeOffset, Reloc},
    ir::Signature,
};
use cranelift_module::{ModuleReloc, ModuleRelocTarget};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ConstBlob, DataIdentity,
    fragment::Code,
    store::{KEY, Store},
    unwind::FunctionUnwind,
};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct Entry {
    /// What the symbol of the function is made from.
    pub(super) label: String,
    pub(super) funcs: Vec<CachedFunc>,
    /// A blob indexes `funcs` and the data before it, as the blobs of `Requests` do.
    pub(super) datas: Vec<CachedData>,
    pub(super) code: CachedCode,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct CachedFunc {
    pub(super) target: CachedTarget,
    pub(super) sig: Signature,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(super) enum CachedTarget {
    /// `checked` when the symbol was one of the program's rustc build, which another
    /// program's may lack.
    Import { symbol: String, checked: bool },
    /// The entry of this name, or whatever else the build has for the instance.
    Named(String),
    /// A function of the checkout that is compiled whenever it is called, by its stable name.
    Root(String),
    /// The `TypeId::of` that returns this.
    TypeId(u128),
    /// The function an `asm!` block is assembled as, by its assembly.
    Asm(String),
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(super) enum CachedData {
    Blob {
        blob: ConstBlob,
        identity: DataIdentity,
    },
    Vtable {
        key: String,
        blob: ConstBlob,
    },
    /// A static of rustc's by its symbol.
    Static {
        symbol: String,
        thread_local: bool,
    },
    /// A static by its stable name, where its symbol is not what tells whose it is: one of
    /// the checkout is rustc's or ours as the build has it, and rustc exports no symbol for
    /// one it kept to its crate.
    OwnStatic(String),
    /// The file, line and column of a `core::panic::Location`.
    Location(Option<(String, u32, u32)>),
    FilePath(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedReloc {
    offset: CodeOffset,
    kind: Reloc,
    name: ModuleRelocTarget,
    addend: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CachedCode {
    signature: Signature,
    bytes: Vec<u8>,
    align: u64,
    relocs: Vec<CachedReloc>,
    unwind: Option<FunctionUnwind>,
}

impl PartialEq for CachedCode {
    fn eq(&self, other: &CachedCode) -> bool {
        // `ModuleRelocTarget` is not `PartialEq`: compare what is written out.
        postcard::to_allocvec(self).ok() == postcard::to_allocvec(other).ok()
    }
}

impl From<&Code> for CachedCode {
    fn from(code: &Code) -> CachedCode {
        CachedCode {
            signature: code.signature.clone(),
            bytes: code.bytes.clone(),
            align: code.align,
            relocs: code
                .relocs
                .iter()
                .map(|it| CachedReloc {
                    offset: it.offset,
                    kind: it.kind,
                    name: it.name.clone(),
                    addend: it.addend,
                })
                .collect(),
            unwind: code.unwind.clone(),
        }
    }
}

impl CachedCode {
    pub(super) fn signature(&self) -> &Signature {
        &self.signature
    }

    pub(super) fn code(&self) -> Code {
        Code {
            signature: self.signature.clone(),
            bytes: self.bytes.clone(),
            align: self.align,
            relocs: self
                .relocs
                .iter()
                .map(|it| ModuleReloc {
                    offset: it.offset,
                    kind: it.kind,
                    name: it.name.clone(),
                    addend: it.addend,
                })
                .collect(),
            unwind: self.unwind.clone(),
        }
    }
}

/// What LLVM made of a function, kept by the IR it was made from. Cranelift keeps its own
/// code the same way, through its incremental cache.
///
/// LLVM asks for things the IR did not name: library functions it calls of its own accord,
/// and tables of addresses. Those are kept here, and relocations against them say which one
/// by its place among them, in a namespace of their own.
#[cfg(feature = "llvm")]
#[derive(Serialize, Deserialize)]
pub(super) struct StoredCode {
    code: CachedCode,
    imports: Vec<String>,
    blobs: Vec<super::ConstBlob>,
}

#[cfg(feature = "llvm")]
impl StoredCode {
    const IMPORTS: u32 = 2;
    const BLOBS: u32 = 3;

    /// The key for `func` compiled at `optimization`. Cranelift's key of the IR leaves out
    /// which requests the IR's names stand for, which LLVM's code has in it.
    pub(super) fn key(
        optimization: super::backend::Optimization,
        func: &cranelift_codegen::ir::Function,
    ) -> Option<[u8; KEY]> {
        let isa = super::target().ok()?;
        let ir = cranelift_codegen::incremental_cache::compute_cache_key(&**isa, func);
        let mut hasher = Sha256::new();
        hasher.update(b"rustd llvm 1");
        hasher.update(super::Backend::Llvm(optimization).to_string());
        hasher.update(ir.to_string());
        for (_, name) in func.params.user_named_funcs().iter() {
            hasher.update(name.namespace.to_le_bytes());
            hasher.update(name.index.to_le_bytes());
        }
        Some(hasher.finalize().into())
    }

    /// `code` as it can be kept, if everything asked for since `mark` is of a kind that is.
    pub(super) fn of(
        code: &Code,
        recorder: &super::fragment::Recorder<'_>,
        mark: super::fragment::Mark,
    ) -> Option<StoredCode> {
        use super::fragment::{DataRequest, FuncTarget};
        let (funcs, datas) = recorder.since(mark);
        let imports = funcs.iter().map(|it| match &it.target {
            FuncTarget::Symbol(symbol) => Some(symbol.clone()),
            _ => None,
        });
        let blobs = datas.iter().map(|it| match it {
            DataRequest::Blob { blob, identity: DataIdentity::Shared } => Some(blob.clone()),
            _ => None,
        });
        let mut stored = StoredCode {
            code: CachedCode::from(code),
            imports: imports.collect::<Option<_>>()?,
            blobs: blobs.collect::<Option<_>>()?,
        };
        for reloc in &mut stored.code.relocs {
            reloc.name = match reloc.name {
                ModuleRelocTarget::User { namespace: 0, index } if index as usize >= mark.funcs => {
                    ModuleRelocTarget::user(Self::IMPORTS, index - mark.funcs as u32)
                }
                ModuleRelocTarget::User { namespace: 1, index } if index as usize >= mark.datas => {
                    ModuleRelocTarget::user(Self::BLOBS, index - mark.datas as u32)
                }
                ref other => other.clone(),
            };
        }
        Some(stored)
    }

    /// The code, with what LLVM asked for asked of `recorder` again.
    pub(super) fn restore(self, recorder: &mut super::fragment::Recorder<'_>) -> Option<Code> {
        let sig = recorder.make_signature();
        let imports: Vec<_> =
            self.imports.iter().map(|it| recorder.import(it, &sig).ok()).collect::<Option<_>>()?;
        let blobs: Vec<_> = self
            .blobs
            .into_iter()
            .map(|it| recorder.define_const(it).ok())
            .collect::<Option<_>>()?;
        let mut code = self.code.code();
        for reloc in &mut code.relocs {
            reloc.name = match reloc.name {
                ModuleRelocTarget::User { namespace: Self::IMPORTS, index } => {
                    ModuleRelocTarget::user(0, imports.get(index as usize)?.as_u32())
                }
                ModuleRelocTarget::User { namespace: Self::BLOBS, index } => {
                    ModuleRelocTarget::user(1, blobs.get(index as usize)?.as_u32())
                }
                ref other => other.clone(),
            };
        }
        Some(code)
    }

    pub(super) fn encode(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<StoredCode> {
        postcard::from_bytes(bytes).ok()
    }
}

impl Entry {
    /// The key of the entry for `name`, or `None` when this build of the compiler cannot be
    /// told from another, whose entries would then be taken for its own.
    pub(super) fn key(name: &str) -> Option<[u8; KEY]> {
        static COMPILER: OnceLock<Option<[u8; KEY]>> = OnceLock::new();
        let compiler = COMPILER.get_or_init(|| {
            let program = std::fs::read(std::env::current_exe().ok()?).ok()?;
            Some(Sha256::digest(program).into())
        });
        let mut hasher = Sha256::new();
        hasher.update(b"rustd entry 3");
        hasher.update((*compiler)?);
        // Code of one producer is not what another would have given.
        hasher.update(super::Backend::from_env().ok()?.to_string());
        // Nor is code for one target what another runs.
        hasher.update(super::Target::selected().ok()?.triple());
        hasher.update([0]);
        hasher.update(name);
        Some(hasher.finalize().into())
    }

    pub(super) fn encode(&self) -> Option<Vec<u8>> {
        postcard::to_allocvec(self).ok()
    }

    pub(super) fn decode(bytes: &[u8]) -> Option<Entry> {
        postcard::from_bytes(bytes).ok()
    }

    /// Whether the program has everything of rustc's that the entry was linked against.
    pub(super) fn links(&self, defines: &dyn Fn(&str) -> bool) -> bool {
        let funcs = self.funcs.iter().all(|it| match &it.target {
            CachedTarget::Import { symbol, checked } => !checked || defines(symbol),
            CachedTarget::Named(_)
            | CachedTarget::Root(_)
            | CachedTarget::TypeId(_)
            | CachedTarget::Asm(_) => true,
        });
        let datas = self.datas.iter().all(|it| match it {
            CachedData::Static { symbol, .. } => defines(symbol),
            _ => true,
        });
        funcs && datas
    }

    fn named(&self) -> impl Iterator<Item = &str> {
        self.funcs.iter().filter_map(|it| match &it.target {
            CachedTarget::Named(name) => Some(name.as_str()),
            CachedTarget::Import { .. }
            | CachedTarget::Root(_)
            | CachedTarget::TypeId(_)
            | CachedTarget::Asm(_) => None,
        })
    }
}

/// The entries of the store that one program was asked about.
pub(super) struct Entries {
    store: &'static Store,
    known: Mutex<Known>,
}

#[derive(Default)]
struct Known {
    entries: FxHashMap<Arc<str>, Option<Arc<Entry>>>,
    /// Whether the program can be built from an entry and everything it calls.
    usable: FxHashMap<Arc<str>, bool>,
}

/// An entry on the way down from the one asked about.
struct Visit {
    name: Arc<str>,
    entry: Arc<Entry>,
    /// How many of the functions the entry refers to have been looked at.
    seen: usize,
    /// The highest visit that this one or one below it calls back into: until that one is
    /// settled, neither is this.
    leans_on: usize,
    /// Where in the list of unsettled entries those below this one start.
    below: usize,
}

impl Entries {
    pub(super) fn new(store: &'static Store) -> Entries {
        Entries { store, known: Mutex::default() }
    }

    /// Never use the entry again in this process: it did not fit the object it was read for.
    pub(super) fn ban(name: &str) {
        Self::banned().lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_owned());
    }

    fn banned() -> &'static Mutex<FxHashSet<String>> {
        static BANNED: OnceLock<Mutex<FxHashSet<String>>> = OnceLock::new();
        BANNED.get_or_init(Mutex::default)
    }

    fn known(&self) -> std::sync::MutexGuard<'_, Known> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn get(&self, name: &str) -> Option<Arc<Entry>> {
        if let Some(known) = self.known().entries.get(name) {
            return known.clone();
        }
        let banned = Self::banned().lock().unwrap_or_else(|e| e.into_inner()).contains(name);
        let entry = (!banned)
            .then(|| Entry::decode(self.store.get(&Entry::key(name)?)?))
            .flatten()
            .map(Arc::new);
        self.known().entries.insert(Arc::from(name), entry.clone());
        entry
    }

    fn settle(&self, name: Arc<str>, usable: bool) {
        self.known().usable.insert(name, usable);
    }

    /// Whether there is an entry for `name` and for everything it calls, and what they import
    /// is in the program, as `defines` tells.
    pub(super) fn usable(&self, name: &str, defines: &dyn Fn(&str) -> bool) -> bool {
        let settled = |name: &str| self.known().usable.get(name).copied();
        if let Some(usable) = settled(name) {
            return usable;
        }
        let name: Arc<str> = Arc::from(name);
        let Some(entry) = self.get(&name).filter(|it| it.links(defines)) else {
            self.settle(name, false);
            return false;
        };
        // Entries call each other in circles, so one is only known to be usable along with
        // the ones it leans on.
        let mut unsettled = vec![name.clone()];
        let mut leaning = FxHashMap::<Arc<str>, usize>::default();
        leaning.insert(name.clone(), 0);
        let mut path = vec![Visit { name, entry, seen: 0, leans_on: 0, below: 0 }];
        while !path.is_empty() {
            let depth = path.len() - 1;
            let visit = &mut path[depth];
            let entry = visit.entry.clone();
            let Some(callee) = entry.named().nth(visit.seen) else {
                let Some(done) = path.pop() else { break };
                if done.leans_on >= depth {
                    for name in unsettled.drain(done.below..) {
                        leaning.remove(&name);
                        self.settle(name, true);
                    }
                } else {
                    leaning.insert(done.name, done.leans_on);
                    let above = &mut path[depth - 1];
                    above.leans_on = above.leans_on.min(done.leans_on);
                }
                continue;
            };
            visit.seen += 1;
            if let Some(&on) = leaning.get(callee) {
                visit.leans_on = visit.leans_on.min(on);
                continue;
            }
            let found = match settled(callee) {
                Some(true) => continue,
                Some(false) => None,
                None => self.get(callee).filter(|it| it.links(defines)),
            };
            let callee: Arc<str> = Arc::from(callee);
            let Some(entry) = found else {
                self.settle(callee, false);
                for visit in path {
                    self.settle(visit.name, false);
                }
                return false;
            };
            leaning.insert(callee.clone(), depth + 1);
            unsettled.push(callee.clone());
            path.push(Visit {
                name: callee,
                entry,
                seen: 0,
                leans_on: depth + 1,
                below: unsettled.len() - 1,
            });
        }
        true
    }
}
