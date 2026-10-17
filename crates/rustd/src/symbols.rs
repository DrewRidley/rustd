//! Maps Rust paths to the mangled names rustc gave them, read from rlibs and objects.

use std::path::{Path, PathBuf};

use anyhow::Context;
use object::{Object, ObjectSymbol, read::archive::ArchiveFile};
use rustc_hash::{FxHashMap, FxHashSet};

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
    /// The crates of the link, each by its name and the file it starts at.
    linked: FxHashSet<(String, PathBuf)>,
    /// Every symbol defined or referred to, hashed: two that hash alike are not expected.
    names: FxHashSet<u64>,
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
        let mut table = SymbolTable::default();
        let mut symbols = Vec::new();
        let mut owners = Vec::new();
        for file in files {
            let data =
                std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
            let first = symbols.len();
            match ArchiveFile::parse(&*data) {
                Ok(archive) => {
                    for member in archive.members() {
                        table.add_object(member?.data(&*data)?, &mut symbols);
                    }
                }
                Err(_) => table.add_object(&data, &mut symbols),
            }
            if let Some((name, source)) = crate_root(file, root) {
                owners.push((name, source, first..symbols.len()));
            }
        }
        // A crate's symbols mention the crates of its name that it depends on (a binary
        // instantiates the generics of its library) besides itself, and never one that depends
        // on it: the id that is left once the others have theirs is its own. A crate may be
        // several files, when it is linked from its objects.
        let mut unplaced: FxHashMap<PathBuf, FxHashMap<u64, usize>> = FxHashMap::default();
        for (name, source, range) in owners {
            table.linked.insert((name.clone(), source.clone()));
            if table.crates.get(&name) != Some(&None) {
                continue;
            }
            let mentions = unplaced.entry(source).or_default();
            for symbol in &symbols[range] {
                for (_, id) in crate_ids(&symbol.with_ids).filter(|it| it.0 == name) {
                    *mentions.entry(id).or_default() += 1;
                }
            }
        }
        table.place(unplaced);
        table.index(symbols);
        Ok(table)
    }

    /// The table of the symbols named, for a link that is not read from files.
    #[allow(dead_code)] // `rustd-wasm` builds this file too, and has no files to read.
    pub(crate) fn of_names<'a>(names: impl IntoIterator<Item = &'a str>) -> SymbolTable {
        let mut table = SymbolTable::default();
        let mut symbols = Vec::new();
        for name in names {
            table.add_name(name, &mut symbols);
        }
        table.index(symbols);
        table
    }

    /// Files the symbols under their paths, once every crate named in one is known.
    fn index(&mut self, symbols: Vec<Symbol>) {
        let table = self;
        for Symbol { name, place, with_ids, path } in symbols {
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
            let known = table.by_path.entry(path).or_default();
            let same = |it: &Placed| {
                without_instantiating_crate(&it.name) == without_instantiating_crate(&name)
            };
            if !known.iter().any(same) {
                known.push(Placed { name, place });
            }
        }
    }

    fn hashed(symbol: &str) -> u64 {
        use std::hash::BuildHasher;
        rustc_hash::FxBuildHasher.hash_one(symbol)
    }

    pub(crate) fn defines(&self, symbol: &str) -> bool {
        self.names.contains(&Self::hashed(symbol))
    }

    fn add_object(&mut self, data: &[u8], symbols: &mut Vec<Symbol>) {
        // rlibs also hold `lib.rmeta`, which isn't an object file.
        let Ok(object) = object::File::parse(data) else { return };
        for symbol in object.symbols() {
            // A reference is as good as a definition: something in the link defines it.
            // Not `is_definition`, which leaves out thread locals.
            // A wasm object's data is in segments, which are not sections.
            let in_segment = object.format() == object::BinaryFormat::Wasm
                && symbol.section() == object::SymbolSection::Unknown;
            let defined = symbol.section_index().is_some() || symbol.is_common() || in_segment;
            let linkable = symbol.is_undefined() || (defined && symbol.is_global());
            if !linkable {
                continue;
            }
            let Ok(name) = symbol.name() else { continue };
            // Mach-O spells every name of C and Rust with an underscore in front, which the
            // writer of an object adds back.
            let name = match object.format() {
                object::BinaryFormat::MachO => name.strip_prefix('_').unwrap_or(name),
                _ => name,
            };
            self.add_name(name, symbols);
        }
    }

    fn add_name(&mut self, name: &str, symbols: &mut Vec<Symbol>) {
        self.names.insert(Self::hashed(name));
        // The memory a static points to is a symbol that demangles to the static's path.
        if name.starts_with("_RNn") {
            return;
        }
        let Ok(demangled) = rustc_demangle::try_demangle(name) else { return };
        let with_ids = demangled.to_string();
        for (krate, id) in crate_ids(&with_ids) {
            match self.crates.get_mut(krate) {
                Some(known) if *known != Some(id) => *known = None,
                Some(_) => {}
                None => {
                    self.crates.insert(krate.to_owned(), Some(id));
                }
            }
        }
        // `{:#}` drops the crate disambiguator hashes.
        symbols.push(Symbol {
            name: name.to_owned(),
            place: V0::place(name),
            with_ids,
            path: format!("{demangled:#}"),
        });
    }

    /// How paths spell the crate named `name` whose source starts at `root`: `name[id]` when
    /// another crate has the name too.
    pub(crate) fn crate_path(&self, name: &str, root: &Path) -> Option<String> {
        match self.crates.get(name) {
            Some(None) => Some(format!("{name}[{:x}]", self.by_root.get(root)?)),
            _ => Some(name.to_owned()),
        }
    }

    /// Gives each crate that shares its name the id that is its own, from how often its symbols
    /// mention each id of that name.
    fn place(&mut self, mut unplaced: FxHashMap<PathBuf, FxHashMap<u64, usize>>) {
        loop {
            let taken: FxHashSet<u64> = self.by_root.values().copied().collect();
            let before = unplaced.len();
            unplaced.retain(|source, mentions| {
                let mut free = mentions.keys().filter(|id| !taken.contains(id));
                match (free.next(), free.next()) {
                    (Some(&id), None) => {
                        self.by_root.insert(source.clone(), id);
                        false
                    }
                    _ => true,
                }
            });
            if unplaced.len() == before {
                break;
            }
        }
        // The other crate of the name is none of these files (a test named `test`, and the
        // sysroot's): a guess, that a crate mentions itself more than it does the other.
        for (source, mentions) in unplaced {
            if let Some((&id, _)) = mentions.iter().max_by_key(|(_, count)| **count) {
                self.by_root.insert(source, id);
            }
        }
    }

    /// Whether the crate named `name` whose source starts at `root` is one of the link. A
    /// bench may be named as the library, start where a test does, and be another program.
    pub(crate) fn links(&self, name: &str, root: &Path) -> bool {
        self.linked.contains(&(name.to_owned(), root.to_owned()))
    }

    pub(crate) fn stable_crate_id(&self, name: &str, root: &Path) -> Option<u64> {
        match *self.crates.get(name)? {
            Some(id) => Some(id),
            None => self.by_root.get(root).copied(),
        }
    }

    /// The symbol of the one item at `path`. A symbol that is alone at its path and is not
    /// the first of its name there is of another item than the one the path was read from:
    /// rustc wrote that item itself, as it does the `main` of a test executable, and built
    /// nothing of the one in the source.
    pub(crate) fn get(&self, path: &str) -> Option<&str> {
        if let Some([only]) = self.by_path.get(path).map(Vec::as_slice)
            && only.place.is_none_or(|it| it == 0)
        {
            return Some(&only.name);
        }
        if let Some((base, _)) = path.rsplit_once("::<")
            && let Some([only]) = self.by_path.get(base).map(Vec::as_slice)
            && only.name.starts_with("_ZN")
            && only.place.is_none_or(|it| it == 0)
        {
            return Some(&only.name);
        }
        None
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
            // Its closures may be numbered otherwise than here, but are as many.
            [only] => {
                return Some(only)
                    .filter(here)
                    .filter(|it| it.closures.len() == closures.len())
                    .map(|it| &*it.name);
            }
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
        // A length of zero is one `0`: the digits after it start the next name.
        let digits = match self.rest.first()? {
            b'0' => 1,
            _ => self.rest.iter().take_while(|it| it.is_ascii_digit()).count(),
        };
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rustc_hash::FxHashMap;

    use super::{SymbolTable, V0};

    fn placed(crates: &[(&str, &[(u64, usize)])]) -> Vec<(String, u64)> {
        let mut table = SymbolTable::default();
        let unplaced: FxHashMap<_, _> = crates
            .iter()
            .map(|(root, mentions)| (PathBuf::from(root), mentions.iter().copied().collect()))
            .collect();
        table.place(unplaced);
        let mut placed: Vec<_> =
            table.by_root.iter().map(|(root, id)| (root.display().to_string(), *id)).collect();
        placed.sort();
        placed
    }

    #[test]
    fn a_binary_that_mentions_its_library_more_than_itself() {
        let placed = placed(&[("src/lib.rs", &[(1, 5)]), ("src/main.rs", &[(1, 9), (2, 3)])]);
        assert_eq!(placed, [("src/lib.rs".to_owned(), 1), ("src/main.rs".to_owned(), 2)]);
    }

    #[test]
    fn a_test_named_as_a_crate_of_the_sysroot() {
        let placed = placed(&[("tests/test.rs", &[(7, 6), (8, 300)])]);
        assert_eq!(placed, [("tests/test.rs".to_owned(), 8)]);
    }

    #[test]
    fn place_of_a_static_in_a_closure() {
        let in_function = "_RNvNvMNtCs7aCYEL8njSF_2h24sendNtB4_4Send5apply10___CALLSITE";
        let first = "_RNvNCNvMNtCs7aCYEL8njSF_2h24sendNtB6_4Send5apply010___CALLSITE";
        let second = "_RNvNCNvMNtCs7aCYEL8njSF_2h24sendNtB6_4Send5apply0s_10___CALLSITE";
        let third = "_RNvNCNvMNtCs7aCYEL8njSF_2h24sendNtB6_4Send5apply0s0_10___CALLSITE";
        assert_eq!(V0::place(in_function), Some(0));
        assert_eq!(V0::place(first), Some(0));
        assert_eq!(V0::place(second), Some(1));
        assert_eq!(V0::place(third), Some(2));
    }
}
