//! Maps Rust paths to the mangled names rustc gave them, read from rlibs and objects.

use std::path::{Path, PathBuf};

use anyhow::Context;
use object::{Object, ObjectSymbol, read::archive::ArchiveFile};
use rustc_hash::FxHashMap;

#[derive(Default)]
pub(crate) struct SymbolTable {
    /// More than one symbol for the items that print as one path: of two `const _` blocks of
    /// a module, or of one name in one function. Linking one to another's symbol is a
    /// miscompile.
    by_path: FxHashMap<String, Vec<Placed>>,
    /// Symbols of items nested in closures and anonymous constants, by their path without the
    /// `{closure#0}`-like segments, with the numbers of the closures among those.
    nested: FxHashMap<String, Vec<NestedSymbol>>,
    /// rustc's `StableCrateId` of each crate named in a symbol. `None` when two crates share
    /// the name, two versions of one package say.
    crates: FxHashMap<String, Option<u64>>,
    /// The `StableCrateId` of the crate whose source starts at a file, for the crates that
    /// share a name. Their paths keep the `name[id]` rustc mangled into the symbol.
    by_root: FxHashMap<PathBuf, u64>,
}

struct NestedSymbol {
    name: String,
    closures: Vec<u32>,
    place: Option<u64>,
}

struct Placed {
    name: String,
    /// The place of the item among those of its name in what it is written in, which rustc
    /// mangles into the symbol and no path says. `None` for a symbol we can't read it from.
    place: Option<u64>,
}

/// A linkable symbol: its mangled name and its path with and without crate ids.
struct Symbol {
    name: String,
    place: Option<u64>,
    with_ids: String,
    path: String,
}

impl SymbolTable {
    /// `root` is what the paths in cargo's dep-info files are relative to.
    pub(crate) fn load(files: &[PathBuf], root: &Path) -> anyhow::Result<SymbolTable> {
        let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
        let next = std::sync::atomic::AtomicUsize::new(0);
        let mut per_file: Vec<(Option<(String, PathBuf)>, Vec<String>)> =
            (0..files.len()).map(|_| (None, Vec::new())).collect();
        std::thread::scope(|scope| {
            let chunk_size = files.len().div_ceil(threads).max(1);
            for (idx, chunk) in per_file.chunks_mut(chunk_size).enumerate() {
                let start = idx * chunk_size;
                scope.spawn(move || {
                    let mut local_seen = rustc_hash::FxHashSet::default();
                    for (offset, slot) in chunk.iter_mut().enumerate() {
                        let file = &files[start + offset];
                        let Ok(data) = std::fs::read(file) else { continue };
                        let mut names = Vec::new();
                        match ArchiveFile::parse(&*data) {
                            Ok(archive) => {
                                for member in archive.members().flatten() {
                                    if let Ok(member_data) = member.data(&*data) {
                                        Self::collect_names(
                                            member_data,
                                            &mut names,
                                            &mut local_seen,
                                        );
                                    }
                                }
                            }
                            Err(_) => Self::collect_names(&data, &mut names, &mut local_seen),
                        }
                        *slot = (crate_root(file, root), names);
                    }
                });
            }
        });
        let _ = next;
        let mut seen = rustc_hash::FxHashSet::default();
        let mut unique_names = Vec::new();
        let mut owners = Vec::new();
        for (owner, names) in per_file {
            let first = unique_names.len();
            for name in names {
                if seen.insert(without_instantiating_crate(&name).to_owned()) {
                    unique_names.push(name);
                }
            }
            if let Some((name, source)) = owner {
                owners.push((name, source, first..unique_names.len()));
            }
        }
        let mut symbols: Vec<Option<Symbol>> = (0..unique_names.len()).map(|_| None).collect();
        std::thread::scope(|scope| {
            let chunk_size = unique_names.len().div_ceil(threads).max(1);
            for (idx, chunk) in symbols.chunks_mut(chunk_size).enumerate() {
                let start = idx * chunk_size;
                let unique_names = &unique_names;
                scope.spawn(move || {
                    for (offset, slot) in chunk.iter_mut().enumerate() {
                        let name = &unique_names[start + offset];
                        let Ok(demangled) = rustc_demangle::try_demangle(name) else { continue };
                        let with_ids = demangled.to_string();
                        let path = format!("{demangled:#}");
                        *slot = Some(Symbol {
                            name: name.clone(),
                            place: V0::place(name),
                            with_ids,
                            path,
                        });
                    }
                });
            }
        });
        let mut table = SymbolTable::default();
        for sym in symbols.iter().flatten() {
            for (krate, id) in crate_ids(&sym.with_ids) {
                match table.crates.get_mut(krate) {
                    Some(known) if *known != Some(id) => *known = None,
                    Some(_) => {}
                    None => {
                        table.crates.insert(krate.to_owned(), Some(id));
                    }
                }
            }
        }
        for (name, source, range) in owners {
            if table.crates.get(&name) != Some(&None) {
                continue;
            }
            // Nothing a crate defines mentions the other crate of its name.
            let id = symbols[range].iter().flatten().find_map(|symbol| {
                crate_ids(&symbol.with_ids).find(|it| it.0 == name).map(|it| it.1)
            });
            if let Some(id) = id {
                table.by_root.insert(source, id);
            }
        }
        for Symbol { name, place, with_ids, path } in symbols.into_iter().flatten() {
            let shared = |krate: &str| table.crates.get(krate) == Some(&None);
            let path = if crate_ids(&with_ids).any(|(krate, _)| shared(krate)) {
                without_ids(&with_ids, |krate| !shared(krate))
            } else {
                path
            };
            // A closure is not an item nested in itself.
            let unnamed = path.rsplit("::").next().is_some_and(|last| last.starts_with('{'));
            if path.contains("::{") && !unnamed {
                let named: Vec<_> = path.split("::").filter(|it| !it.starts_with('{')).collect();
                let closures = path
                    .split("::")
                    .filter_map(|it| it.strip_prefix("{closure#")?.strip_suffix('}')?.parse().ok())
                    .collect();
                let symbols = table.nested.entry(named.join("::")).or_default();
                if symbols.iter().all(|it| it.name != name) {
                    symbols.push(NestedSymbol { name: name.clone(), closures, place });
                }
            }
            table.by_path.entry(path).or_default().push(Placed { name, place });
        }
        Ok(table)
    }

    fn collect_names(
        data: &[u8],
        names: &mut Vec<String>,
        seen: &mut rustc_hash::FxHashSet<String>,
    ) {
        // rlibs also hold `lib.rmeta`, which isn't an object file.
        let Ok(object) = object::File::parse(data) else { return };
        let is_macho = object.format() == object::BinaryFormat::MachO;
        for symbol in object.symbols() {
            // A reference is as good as a definition: something in the link defines it.
            // Not `is_definition`, which leaves out thread locals.
            let defined = symbol.section_index().is_some() || symbol.is_common();
            let linkable = symbol.is_undefined() || (defined && symbol.is_global());
            if !linkable {
                continue;
            }
            let Ok(name) = symbol.name() else { continue };
            let name = if is_macho { name.strip_prefix('_').unwrap_or(name) } else { name };
            // The memory a static points to is a symbol that demangles to the static's path.
            let is_v0 = name.starts_with("_R") && !name.starts_with("_RNn");
            let is_legacy = name.starts_with("_ZN")
                && name.ends_with('E')
                && name.len() > 20
                && &name.as_bytes()[name.len() - 20..name.len() - 17] == b"17h";
            if !is_v0 && !is_legacy {
                continue;
            }
            let base = without_instantiating_crate(name);
            if seen.insert(base.to_owned()) {
                names.push(name.to_owned());
            }
        }
    }

    /// How paths spell the crate named `name` whose source starts at `root`: `name[id]` when
    /// another crate has the name too.
    pub(crate) fn crate_path(&self, name: &str, root: &Path) -> Option<String> {
        match self.crates.get(name) {
            Some(None) => Some(format!("{name}[{:x}]", self.by_root.get(root)?)),
            _ => Some(name.to_owned()),
        }
    }

    pub(crate) fn stable_crate_id(&self, name: &str, root: &Path) -> Option<u64> {
        match *self.crates.get(name)? {
            Some(id) => Some(id),
            None => self.by_root.get(root).copied(),
        }
    }

    pub(crate) fn get(&self, path: &str) -> Option<&str> {
        match &**self.by_path.get(path)? {
            [only] => Some(&only.name),
            _ => None,
        }
    }

    /// The symbol of the item at `path` that is the `place`th of its name in what it is
    /// written in.
    pub(crate) fn get_at(&self, path: &str, place: u64) -> Option<&str> {
        let mut symbols = match &**self.by_path.get(path)? {
            [only] => {
                return Some(only)
                    .filter(|it| it.place.is_none_or(|it| it == place))
                    .map(|it| &*it.name);
            }
            symbols => symbols.iter().filter(|it| it.place == Some(place)),
        };
        let symbol = symbols.next()?;
        symbols.next().is_none().then_some(&*symbol.name)
    }

    /// The symbol of an item whose path has unnamed segments between the named ones given.
    ///
    /// `closures` are the numbers rustc gives the closures around the item, outermost first,
    /// which tell apart the items that only differ in those, and `place` is as in `get_at`.
    /// Anonymous constants are not numbered, so two items that differ in one of those have
    /// no symbol here.
    pub(crate) fn get_nested(&self, named: &str, closures: &[u32], place: u64) -> Option<&str> {
        let here = |it: &&NestedSymbol| it.place.is_none_or(|known| known == place);
        let mut symbols = match &**self.nested.get(named)? {
            [only] => return Some(only).filter(here).map(|it| &*it.name),
            symbols => symbols.iter().filter(here).filter(|it| it.closures == closures),
        };
        let symbol = symbols.next()?;
        symbols.next().is_none().then_some(&*symbol.name)
    }

    pub(crate) fn len(&self) -> usize {
        self.by_path.len()
    }
}

/// The name of the crate in `libname-hash.rlib`, or in the `name-hash.<unit>.rcgu.o` of a crate
/// built as a test binary, and the file its source starts at, which the dep-info file cargo
/// leaves next to it lists first.
fn crate_root(rlib: &Path, root: &Path) -> Option<(String, PathBuf)> {
    let file = rlib.file_name()?.to_str()?;
    let stem = match file.strip_suffix(".rlib") {
        Some(stem) => stem.strip_prefix("lib")?,
        None => file.strip_suffix(".rcgu.o")?.split_once('.')?.0,
    };
    let (name, _) = stem.rsplit_once('-')?;
    let dep_info = std::fs::read_to_string(rlib.with_file_name(format!("{stem}.d"))).ok()?;
    let (_, sources) = dep_info.lines().next()?.split_once(": ")?;
    // FIXME: a path with a space in it is escaped.
    let source = sources.split(' ').next()?;
    Some((name.to_owned(), root.join(source)))
}

/// A v0 symbol without the crate that instantiated it, which ends the symbol of a generic
/// instance as `Cs<id>_<len><name>` or as a back reference `B<offset>_`. Copies of one instance
/// made by two crates differ only in that.
fn without_instantiating_crate(symbol: &str) -> &str {
    let is_base62 = |c: char| c.is_ascii_alphanumeric();
    if let Some(body) = symbol.strip_suffix('_')
        && let Some(start) = body.rfind('B')
        && body[start + 1..].chars().all(is_base62)
    {
        return &symbol[..start];
    }
    // `Cs<id>_<len><name>`: the name is the last `len` bytes.
    let Some(crate_start) = symbol.rfind("Cs") else { return symbol };
    let Some((id, ident)) = symbol[crate_start + 2..].split_once('_') else { return symbol };
    let digits = ident.chars().take_while(char::is_ascii_digit).count();
    let named = ident[..digits].parse() == Ok(ident.len() - digits);
    if id.chars().all(is_base62) && named { &symbol[..crate_start] } else { symbol }
}

/// The `name[id]` a demangled v0 symbol prints for each crate in it.
fn crate_ids(demangled: &str) -> impl Iterator<Item = (&str, u64)> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut rest = demangled;
    std::iter::from_fn(move || {
        loop {
            let (before, after) = rest.split_once('[')?;
            rest = after;
            let Some((id, _)) = after.split_once(']') else { continue };
            let name_len = before.chars().rev().take_while(|&c| is_ident(c)).count();
            let name = &before[before.len() - name_len..];
            let Ok(id) = u64::from_str_radix(id, 16) else { continue };
            if !name.is_empty() {
                return Some((name, id));
            }
        }
    })
}

/// The demangled symbol without the ids of the crates `drop` picks.
fn without_ids(demangled: &str, drop: impl Fn(&str) -> bool) -> String {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut path = String::with_capacity(demangled.len());
    let mut rest = demangled;
    while let Some(open) = rest.find('[') {
        let (before, after) = rest.split_at(open);
        path.push_str(before);
        let name_len = before.chars().rev().take_while(|&c| is_ident(c)).count();
        let name = &before[before.len() - name_len..];
        match after.find(']') {
            Some(close)
                if !name.is_empty() && u64::from_str_radix(&after[1..close], 16).is_ok() =>
            {
                if !drop(name) {
                    path.push_str(&after[..=close]);
                }
                rest = &after[close + 1..];
            }
            _ => {
                path.push('[');
                rest = &after[1..];
            }
        }
    }
    path.push_str(rest);
    path
}

/// A reader of v0 symbols, for what `rustc_demangle` reads and does not print.
struct V0<'a> {
    rest: &'a [u8],
}

impl V0<'_> {
    /// The disambiguator of the last name in the path of a symbol: `0` for the first item of
    /// that name in what it is written in, `1` for the next.
    fn place(symbol: &str) -> Option<u64> {
        V0 { rest: symbol.strip_prefix("_R")?.as_bytes() }.path()
    }

    fn next(&mut self) -> Option<u8> {
        let (&first, rest) = self.rest.split_first()?;
        self.rest = rest;
        Some(first)
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.rest.first() == Some(&byte);
        if found {
            self.rest = &self.rest[1..];
        }
        found
    }

    fn base62(&mut self) -> Option<u64> {
        if self.eat(b'_') {
            return Some(0);
        }
        let mut value = 0u64;
        loop {
            let digit = match self.next()? {
                b'_' => return value.checked_add(1),
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'z' => byte - b'a' + 10,
                byte @ b'A'..=b'Z' => byte - b'A' + 36,
                _ => return None,
            };
            value = value.checked_mul(62)?.checked_add(digit.into())?;
        }
    }

    fn disambiguator(&mut self) -> Option<u64> {
        if self.eat(b's') { self.base62()?.checked_add(1) } else { Some(0) }
    }

    fn identifier(&mut self) -> Option<()> {
        self.eat(b'u');
        let digits = self.rest.iter().take_while(|it| it.is_ascii_digit()).count();
        let len: usize = str::from_utf8(&self.rest[..digits]).ok()?.parse().ok()?;
        self.rest = &self.rest[digits..];
        self.eat(b'_');
        self.rest = self.rest.get(len..)?;
        Some(())
    }

    /// Reads a path, for the disambiguator of the name it ends in.
    fn path(&mut self) -> Option<u64> {
        match self.next()? {
            b'C' => {
                self.disambiguator()?;
                self.identifier()?;
                Some(0)
            }
            b'M' => {
                self.disambiguator()?;
                self.path()?;
                self.ty()?;
                Some(0)
            }
            b'X' => {
                self.disambiguator()?;
                self.path()?;
                self.ty()?;
                self.path()?;
                Some(0)
            }
            b'Y' => {
                self.ty()?;
                self.path()?;
                Some(0)
            }
            b'N' => {
                self.next()?;
                self.path()?;
                let place = self.disambiguator()?;
                self.identifier()?;
                Some(place)
            }
            b'I' => {
                let place = self.path()?;
                while !self.eat(b'E') {
                    match self.rest.first()? {
                        b'L' => {
                            self.next();
                            self.base62()?;
                        }
                        b'K' => {
                            self.next();
                            self.constant()?;
                        }
                        _ => self.ty()?,
                    }
                }
                Some(place)
            }
            b'B' => {
                self.base62()?;
                Some(0)
            }
            _ => None,
        }
    }

    fn binder(&mut self) -> Option<()> {
        if self.eat(b'G') {
            self.base62()?;
        }
        Some(())
    }

    fn ty(&mut self) -> Option<()> {
        match *self.rest.first()? {
            b'C' | b'M' | b'X' | b'Y' | b'N' | b'I' | b'B' => return self.path().map(drop),
            _ => {}
        }
        match self.next()? {
            b'A' => {
                self.ty()?;
                self.constant()?;
            }
            b'S' | b'P' | b'O' => self.ty()?,
            b'T' => {
                while !self.eat(b'E') {
                    self.ty()?;
                }
            }
            b'R' | b'Q' => {
                if self.eat(b'L') {
                    self.base62()?;
                }
                self.ty()?;
            }
            b'F' => {
                self.binder()?;
                self.eat(b'U');
                if self.eat(b'K') && !self.eat(b'C') {
                    self.identifier()?;
                }
                while !self.eat(b'E') {
                    self.ty()?;
                }
                self.ty()?;
            }
            b'D' => {
                self.binder()?;
                while !self.eat(b'E') {
                    self.path()?;
                    while self.eat(b'p') {
                        self.identifier()?;
                        self.ty()?;
                    }
                }
                if !self.eat(b'L') {
                    return None;
                }
                self.base62()?;
            }
            // A basic type is one lowercase letter.
            byte if byte.is_ascii_lowercase() => {}
            _ => return None,
        }
        Some(())
    }

    // FIXME: the constants of `adt_const_params` are not read.
    fn constant(&mut self) -> Option<()> {
        if self.eat(b'p') {
            return Some(());
        }
        if self.eat(b'B') {
            return self.base62().map(drop);
        }
        self.ty()?;
        while self.next()? != b'_' {}
        Some(())
    }
}
