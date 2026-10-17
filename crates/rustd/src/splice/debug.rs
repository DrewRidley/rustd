//! What a debugger reads of a spliced executable: the lines of the patch's code.
//!
//! The sections a debugger reads are not loaded, and each is one run of the file that whoever
//! reads it finds through the section headers. A section cannot be added to where it is, the
//! next one follows it, so each section the patch has a part of is written again behind the
//! program headers: the executable's, then the patch's, which describes a unit of its own and
//! refers to the other sections by how far into them its part now is. The section headers
//! follow, with those sections where they now are.
//!
//! What the executable says of the functions the patch replaces stays: it is true of their
//! old code, which is still there behind the jump.

use anyhow::{Context, bail, ensure};
use object::{
    Endianness, Object, ObjectSection, ObjectSymbol, RelocationFlags, RelocationTarget,
    SectionIndex, SymbolKind, elf,
    read::elf::{ElfFile64, FileHeader, SectionHeader},
};
use rustc_hash::FxHashMap;

use super::{Machine, Patch, Target};

/// The section headers of an executable, and the sections among them a debugger reads.
pub(super) struct Described {
    headers: Vec<elf::SectionHeader64<Endianness>>,
    sections: FxHashMap<Box<[u8]>, usize>,
}

/// A run of the file that is written again further on.
pub(super) struct Moved {
    pub(super) from: u64,
    pub(super) len: u64,
    pub(super) to: u64,
}

/// What a splice writes behind everything else for a debugger.
#[derive(Default)]
pub(super) struct Tail {
    pub(super) moved: Vec<Moved>,
    pub(super) written: Vec<(u64, Vec<u8>)>,
    /// Where the section headers are, when they are among what is written.
    pub(super) headers_offset: Option<u64>,
}

impl Described {
    pub(super) fn read(file: &ElfFile64<'_, Endianness>) -> anyhow::Result<Described> {
        let endian = file.endian();
        let headers = file.elf_header().section_headers(endian, file.data())?.to_vec();
        let mut sections = FxHashMap::default();
        for section in file.sections() {
            let name = section.name_bytes()?;
            if is_debug(name) {
                sections.insert(name.into(), section.index().0);
            }
        }
        Ok(Described { headers, sections })
    }

    fn says_lines(&self) -> bool {
        self.sections.contains_key(&b".debug_info"[..])
    }
}

fn is_debug(name: &[u8]) -> bool {
    name.starts_with(b".debug_")
}

impl Machine {
    /// How many bytes the place in a section a debugger reads takes, which a relocation of
    /// the type fills in.
    fn described(self, r_type: u32) -> anyhow::Result<usize> {
        Ok(match (self, r_type) {
            (Machine::X86_64, elf::R_X86_64_64) | (Machine::Aarch64, elf::R_AARCH64_ABS64) => 8,
            (Machine::X86_64, elf::R_X86_64_32) | (Machine::Aarch64, elf::R_AARCH64_ABS32) => 4,
            (_, other) => bail!("a relocation of type {other} in what a debugger reads"),
        })
    }
}

impl Patch<'_> {
    /// The sections of the patch a debugger reads.
    pub(super) fn described(
        &self,
    ) -> impl Iterator<Item = object::read::elf::ElfSection64<'_, '_, Endianness>> {
        self.file.sections().filter(|it| it.size() != 0 && it.name_bytes().is_ok_and(is_debug))
    }

    /// What goes from `offset` of the file on so that a debugger finds the lines of the
    /// patch's code, whose addresses `address` gives. Nothing, for an executable that
    /// tells a debugger nothing.
    pub(super) fn describe(
        &self,
        described: &Described,
        machine: Machine,
        offset: u64,
        address: impl Fn(Target, i64) -> anyhow::Result<u64>,
    ) -> anyhow::Result<Tail> {
        let e = Endianness::Little;
        let mut tail = Tail::default();
        if !described.says_lines() || self.described().next().is_none() {
            return Ok(tail);
        }
        let mut headers = described.headers.clone();
        let mut theirs = FxHashMap::<SectionIndex, u64>::default();
        for section in self.described() {
            let name = section.name_bytes()?;
            let shown = String::from_utf8_lossy(name);
            let header = described.sections.get(name).map(|&index| &headers[index]);
            let header = header.with_context(|| format!("the executable has no {shown}"))?;
            ensure!(
                header.sh_flags(e) & u64::from(elf::SHF_COMPRESSED) == 0,
                "the executable's {shown} is compressed"
            );
            theirs.insert(section.index(), header.sh_size(e));
        }
        let mut end = offset;
        for section in self.described() {
            let mut data = section.data()?.to_vec();
            for (at, relocation) in section.relocations() {
                let RelocationFlags::Elf { r_type } = relocation.flags() else {
                    bail!("a relocation that is not ELF's")
                };
                let RelocationTarget::Symbol(index) = relocation.target() else {
                    bail!("a relocation against no symbol")
                };
                let symbol = self.file.symbol_by_index(index)?;
                let within = symbol.section_index().and_then(|it| theirs.get(&it));
                let value = match within {
                    Some(len) if symbol.kind() == SymbolKind::Section => {
                        len.wrapping_add_signed(relocation.addend())
                    }
                    Some(_) => bail!("a symbol in what a debugger reads"),
                    None => {
                        let target = self.targets.get(&index).context("code nothing else names")?;
                        address(*target, relocation.addend())?
                    }
                };
                let len = machine.described(r_type)?;
                let field = data.get_mut(at as usize..at as usize + len);
                let field = field.context("a relocation past its section")?;
                if len == 4 {
                    let value = u32::try_from(value).context("more than a debugger is told of")?;
                    field.copy_from_slice(&value.to_le_bytes());
                } else {
                    field.copy_from_slice(&value.to_le_bytes());
                }
            }
            let name = section.name_bytes()?;
            let header = &mut headers[described.sections[name]];
            let len = header.sh_size(e);
            tail.moved.push(Moved { from: header.sh_offset(e), len, to: end });
            header.sh_offset.set(e, end);
            header.sh_size.set(e, len + data.len() as u64);
            tail.written.push((end + len, data));
            end += header.sh_size(e);
        }
        let end = end.next_multiple_of(8);
        tail.written.push((end, object::bytes_of_slice(&headers).to_vec()));
        tail.headers_offset = Some(end);
        Ok(tail)
    }
}
