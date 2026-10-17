//! The object file LLVM wrote for one function, read back as a [`Code`].
//!
//! The code is the function's section. Constants LLVM put in sections of their own (jump
//! tables, float literals) are laid out after it in the same bytes, and what the code says
//! about where they are is filled in here, so that the rest of rustd sees one piece of code
//! that refers to nothing but what the function asked for.

use cranelift_codegen::{
    binemit::Reloc,
    gimli::{
        self, BaseAddresses, CieOrFde, DW_EH_PE_omit, DW_EH_PE_uleb128, EhFrame, EndianSlice,
        LittleEndian, Reader, UnwindSection,
    },
    ir::{LibCall, Signature},
};
use cranelift_module::{DataId, FuncId, ModuleReloc, ModuleRelocTarget, default_libcall_names};
use object::{
    Architecture, BinaryFormat, Object, ObjectSection, ObjectSymbol, RelocationFlags,
    RelocationTarget, SectionIndex, SectionKind, SymbolSection, elf, macho, pe,
};
use rustc_hash::FxHashMap;

use super::{OWN, Symbol, Unhandled};
use crate::mir::codegen::{
    ConstBlob,
    fragment::{Code, Recorder},
    unwind::{FrameRows, FrameRule, FunctionUnwind, seh},
};

pub(super) struct Emitted<'data> {
    file: object::File<'data>,
    /// What became of each section the code leads to.
    placed: FxHashMap<SectionIndex, Placement>,
    /// The sections that are part of the code, in the order they are in `bytes`.
    inline: Vec<SectionIndex>,
    bytes: Vec<u8>,
    /// How long the function is at the start of `bytes`.
    code: u32,
    align: u64,
    relocs: Vec<ModuleReloc>,
}

#[derive(Clone, Copy)]
enum Placement {
    /// Part of the code, this many bytes in.
    Inline(u64),
    /// A data object of its own: constants that hold addresses, which code cannot.
    Blob(DataId),
}

impl<'data> Emitted<'data> {
    pub(super) fn code(
        object: &'data [u8],
        signature: Signature,
        recorder: &mut Recorder<'_>,
    ) -> Result<Code, Unhandled> {
        let file = object::File::parse(object)?;
        let own = file
            .symbols()
            .find(|symbol| {
                symbol.is_definition() && symbol.name().is_ok_and(|name| plain(&file, name) == OWN)
            })
            .ok_or(Unhandled::Shape("an object without the function"))?;
        let SymbolSection::Section(text) = own.section() else {
            return Err(Unhandled::Shape("an object without the function"));
        };
        if own.address() != 0 {
            return Err(Unhandled::Shape("code ahead of the function"));
        }
        let mut emitted = Emitted {
            file,
            placed: FxHashMap::default(),
            inline: Vec::new(),
            bytes: Vec::new(),
            code: 0,
            align: 1,
            relocs: Vec::new(),
        };
        let unwind = match (emitted.file.format(), emitted.file.architecture()) {
            (BinaryFormat::MachO, _) => {
                emitted.place_paged(text, recorder)?;
                emitted.relocate_paged(recorder)?;
                emitted.unwind_macho()?
            }
            (BinaryFormat::Elf, Architecture::Aarch64) => {
                emitted.place_paged(text, recorder)?;
                emitted.relocate_paged(recorder)?;
                emitted.unwind()?
            }
            (BinaryFormat::Coff, _) => {
                emitted.place(text, recorder)?;
                emitted.relocate(recorder)?;
                emitted.unwind_coff()?
            }
            _ => {
                emitted.place(text, recorder)?;
                emitted.relocate(recorder)?;
                emitted.unwind()?
            }
        };
        let Emitted { bytes, align, relocs, .. } = emitted;
        Ok(Code { signature, bytes, align, relocs, unwind })
    }

    /// Fills the function, which is all of `code`, up to the length of the shortest one
    /// there may be, and answers how long it is then: what LLVM makes of an empty body is a
    /// `ret` alone.
    fn lengthen(code: &mut Vec<u8>) -> Result<u32, Unhandled> {
        let target = crate::mir::codegen::Target::selected()
            .map_err(|_| Unhandled::Shape("a target that is not known"))?;
        if let Some((shortest, filler)) = target.arch.shortest_function()
            && code.len() < shortest
        {
            code.resize(shortest, filler);
        }
        Ok(code.len() as u32)
    }

    /// Lays out `first` and every section its relocations lead to.
    fn place(&mut self, first: SectionIndex, recorder: &mut Recorder<'_>) -> Result<(), Unhandled> {
        let mut pending = vec![first];
        while let Some(index) = pending.pop() {
            if self.placed.contains_key(&index) {
                continue;
            }
            let section = self.file.section_by_index(index)?;
            if section.relocations().any(|(_, relocation)| Self::is_address(&relocation)) {
                let blob = self.blob(&section)?;
                let id = recorder
                    .define_const(blob)
                    .map_err(|_| Unhandled::Shape("constants that cannot be defined"))?;
                self.placed.insert(index, Placement::Blob(id));
                continue;
            }
            match section.kind() {
                SectionKind::Text
                | SectionKind::ReadOnlyData
                | SectionKind::ReadOnlyString
                | SectionKind::ReadOnlyDataWithRel => {}
                _ => return Err(Unhandled::Shape("code that refers to a writable section")),
            }
            let align = section.align().max(1);
            self.align = self.align.max(align);
            let start = (self.bytes.len() as u64).next_multiple_of(align);
            self.bytes.resize(start as usize, 0);
            self.bytes.extend_from_slice(section.data()?);
            if index == first {
                self.code = Self::lengthen(&mut self.bytes)?;
            }
            self.placed.insert(index, Placement::Inline(start));
            self.inline.push(index);
            for (_, relocation) in section.relocations() {
                if let SymbolSection::Section(target) =
                    Self::symbol(&self.file, &relocation)?.section()
                {
                    pending.push(target);
                }
            }
        }
        Ok(())
    }

    /// A section of addresses of what the function asked for, such as the table LLVM makes
    /// of a `match` that picks one of several constants.
    fn blob(&self, section: &object::Section<'data, '_>) -> Result<ConstBlob, Unhandled> {
        // Written to only by the loader, which fills in the addresses. A PE image has them
        // with the rest of what is only read.
        let read_only = match self.file.format() {
            BinaryFormat::Coff => section.kind() == SectionKind::ReadOnlyData,
            _ => section.name()?.starts_with(".data.rel.ro"),
        };
        if !read_only {
            return Err(Unhandled::Shape("code that refers to a writable section"));
        }
        let mut blob = ConstBlob {
            bytes: section.data()?.to_vec(),
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: section.align().max(1),
        };
        for (offset, relocation) in section.relocations() {
            let symbol = Self::symbol(&self.file, &relocation)?;
            let named = symbol.is_undefined().then(|| symbol.name().ok()).flatten();
            let target = named.and_then(Symbol::parse);
            let absolute = Self::is_address(&relocation);
            // COFF keeps what is added to the address where the address goes.
            let held = match blob.bytes.get_mut(offset as usize..).and_then(<[u8]>::first_chunk_mut)
            {
                Some(field) if absolute && relocation.has_implicit_addend() => {
                    i64::from_le_bytes(std::mem::take(field))
                }
                _ => 0,
            };
            match (target, relocation.addend() + held) {
                (Some(Symbol::Data(index)), addend) if absolute => {
                    blob.relocs.push((offset as u32, DataId::from_u32(index), addend));
                }
                (Some(Symbol::Func(index)), 0) if absolute => {
                    blob.fn_relocs.push((offset as u32, FuncId::from_u32(index)));
                }
                _ => return Err(Unhandled::Shape("constants that hold an address of the code")),
            }
        }
        Ok(blob)
    }

    /// Whether the relocation asks for an address, in the eight bytes one takes.
    fn is_address(relocation: &object::Relocation) -> bool {
        matches!(
            relocation.flags(),
            RelocationFlags::Elf { r_type: elf::R_X86_64_64 | elf::R_AARCH64_ABS64 }
                | RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_ADDR64 }
        )
    }

    fn symbol<'file>(
        file: &'file object::File<'data>,
        relocation: &object::Relocation,
    ) -> Result<object::Symbol<'data, 'file>, Unhandled> {
        let RelocationTarget::Symbol(symbol) = relocation.target() else {
            return Err(Unhandled::Shape("a relocation against no symbol"));
        };
        Ok(file.symbol_by_index(symbol)?)
    }

    /// Fills in what points from one part of the code into another, and turns the rest into
    /// relocations against requests.
    fn relocate(&mut self, recorder: &mut Recorder<'_>) -> Result<(), Unhandled> {
        for &index in &self.inline {
            let section = self.file.section_by_index(index)?;
            let Placement::Inline(base) = self.placed[&index] else { continue };
            for (offset, relocation) in section.relocations() {
                let at = base + offset;
                let (field, addend) = Self::field(&mut self.bytes, at, &relocation)?;
                let symbol = Self::symbol(&self.file, &relocation)?;
                let within = match symbol.section() {
                    SymbolSection::Section(target) => Some(self.placed[&target]),
                    _ => None,
                };
                if let Some(Placement::Inline(target)) = within {
                    // Both ends are in `bytes`, whose pieces do not move against each other.
                    let (Field::Near | Field::Call) = field else {
                        return Err(Unhandled::Shape("code that holds an address of itself"));
                    };
                    let target = target + symbol.address();
                    let value = target.wrapping_add_signed(addend).wrapping_sub(at);
                    Self::fill(&mut self.bytes, at, value as u32)?;
                    continue;
                }
                let kind = match field {
                    Field::Call => Reloc::X86CallPLTRel4,
                    Field::Near => Reloc::X86PCRel4,
                    Field::Got => Reloc::X86GOTPCRel4,
                    Field::TlsGd => Reloc::ElfX86_64TlsGd,
                    Field::SectionOffset => Reloc::X86SecRel,
                };
                let (name, addend) = match within {
                    Some(Placement::Blob(id)) => {
                        (ModuleRelocTarget::user(1, id.as_u32()), addend + symbol.address() as i64)
                    }
                    _ => (Self::target(symbol.name()?, recorder)?, addend),
                };
                self.relocs.push(ModuleReloc { offset: at as u32, kind, name, addend });
            }
        }
        Ok(())
    }

    /// What the relocation of the four bytes of `bytes` at `at` asks for, and what is added to the
    /// address it names, counted from the field as ELF counts it. COFF keeps that in the
    /// field, which is cleared: whoever applies the relocation is given it apart.
    fn field(
        bytes: &mut [u8],
        at: u64,
        relocation: &object::Relocation,
    ) -> Result<(Field, i64), Unhandled> {
        let field = match relocation.flags() {
            RelocationFlags::Elf { r_type } => match r_type {
                elf::R_X86_64_PLT32 => Field::Call,
                elf::R_X86_64_PC32 => Field::Near,
                // The relaxable forms are loads from the GOT like any other.
                elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX | elf::R_X86_64_REX_GOTPCRELX => {
                    Field::Got
                }
                elf::R_X86_64_TLSGD => Field::TlsGd,
                other => return Err(Unhandled::Relocation(other)),
            },
            RelocationFlags::Coff { typ } => match typ {
                // The numbered forms say how many bytes of the instruction follow the
                // field, which `object` has put in the addend.
                pe::IMAGE_REL_AMD64_REL32
                | pe::IMAGE_REL_AMD64_REL32_1
                | pe::IMAGE_REL_AMD64_REL32_2
                | pe::IMAGE_REL_AMD64_REL32_3
                | pe::IMAGE_REL_AMD64_REL32_4
                | pe::IMAGE_REL_AMD64_REL32_5 => Field::Near,
                pe::IMAGE_REL_AMD64_SECREL => Field::SectionOffset,
                other => return Err(Unhandled::Relocation(other.into())),
            },
            _ => return Err(Unhandled::Shape("an object of a format that is not read")),
        };
        if !relocation.has_implicit_addend() {
            return Ok((field, relocation.addend()));
        }
        let held = Self::fill(bytes, at, 0)?;
        Ok((field, relocation.addend() + i64::from(held as i32)))
    }

    /// Puts `value` in the four bytes of `bytes` at `at` and answers what was there.
    fn fill(bytes: &mut [u8], at: u64, value: u32) -> Result<u32, Unhandled> {
        let field = usize::try_from(at)
            .ok()
            .and_then(|at| bytes.get_mut(at..)?.first_chunk_mut())
            .ok_or(Unhandled::Shape("a relocation outside its section"))?;
        Ok(u32::from_le_bytes(std::mem::replace(field, value.to_le_bytes())))
    }

    /// Lays out the function's section. Every other section its relocations lead to is a
    /// data object of its own: AArch64 code finds what is near it by the page it is on,
    /// which only the linker knows.
    fn place_paged(
        &mut self,
        text: SectionIndex,
        recorder: &mut Recorder<'_>,
    ) -> Result<(), Unhandled> {
        let section = self.file.section_by_index(text)?;
        self.align = section.align().max(1);
        self.bytes.extend_from_slice(section.data()?);
        self.code = Self::lengthen(&mut self.bytes)?;
        self.placed.insert(text, Placement::Inline(0));
        self.inline.push(text);
        for (_, relocation) in section.relocations() {
            let SymbolSection::Section(index) = Self::symbol(&self.file, &relocation)?.section()
            else {
                continue;
            };
            if self.placed.contains_key(&index) {
                continue;
            }
            let section = self.file.section_by_index(index)?;
            let blob = match self.file.format() {
                BinaryFormat::MachO => self.blob_macho(&section)?,
                _ => self.blob_elf(&section)?,
            };
            let id = recorder
                .define_const(blob)
                .map_err(|_| Unhandled::Shape("constants that cannot be defined"))?;
            self.placed.insert(index, Placement::Blob(id));
        }
        Ok(())
    }

    /// A section of constants, which may hold addresses of what the function asked for.
    fn blob_elf(&self, section: &object::Section<'data, '_>) -> Result<ConstBlob, Unhandled> {
        if section.relocations().next().is_some() {
            return self.blob(section);
        }
        match section.kind() {
            SectionKind::ReadOnlyData | SectionKind::ReadOnlyString => Ok(ConstBlob {
                bytes: section.data()?.to_vec(),
                relocs: Vec::new(),
                fn_relocs: Vec::new(),
                align: section.align().max(1),
            }),
            _ => Err(Unhandled::Shape("code that refers to a writable section")),
        }
    }

    /// A section of constants, which may hold addresses of what the function asked for.
    fn blob_macho(&self, section: &object::Section<'data, '_>) -> Result<ConstBlob, Unhandled> {
        match section.kind() {
            SectionKind::ReadOnlyData
            | SectionKind::ReadOnlyString
            | SectionKind::ReadOnlyDataWithRel => {}
            // `__DATA,__const`: written to only by the loader, which fills in the addresses.
            SectionKind::Data if section.name()? == "__const" => {}
            _ => return Err(Unhandled::Shape("code that refers to a writable section")),
        }
        let mut blob = ConstBlob {
            bytes: section.data()?.to_vec(),
            relocs: Vec::new(),
            fn_relocs: Vec::new(),
            align: section.align().max(1),
        };
        const HELD: Unhandled = Unhandled::Shape("constants that hold an address of the code");
        for (offset, relocation) in section.relocations() {
            let RelocationFlags::MachO { r_type: macho::ARM64_RELOC_UNSIGNED, r_length: 3, .. } =
                relocation.flags()
            else {
                return Err(HELD);
            };
            let symbol = Self::symbol(&self.file, &relocation)?;
            let named = symbol.is_undefined().then(|| symbol.name().ok()).flatten();
            // The addend is where the address will be.
            let at = offset as usize;
            let field = blob.bytes.get_mut(at..at + 8).ok_or(HELD)?;
            let addend = i64::from_le_bytes((&*field).try_into().map_err(|_| HELD)?);
            field.fill(0);
            match (named.and_then(|name| Symbol::parse(plain(&self.file, name))), addend) {
                (Some(Symbol::Data(index)), addend) => {
                    blob.relocs.push((at as u32, DataId::from_u32(index), addend));
                }
                (Some(Symbol::Func(index)), 0) => {
                    blob.fn_relocs.push((at as u32, FuncId::from_u32(index)));
                }
                _ => return Err(HELD),
            }
        }
        Ok(blob)
    }

    /// Turns the relocations of the function into relocations against requests and against
    /// the data objects its constants became.
    fn relocate_paged(&mut self, recorder: &mut Recorder<'_>) -> Result<(), Unhandled> {
        let section = self.file.section_by_index(self.inline[0])?;
        for (offset, relocation) in section.relocations() {
            let kind = match relocation.flags() {
                RelocationFlags::MachO { r_type, .. } => match r_type {
                    macho::ARM64_RELOC_BRANCH26 => Reloc::Arm64Call,
                    macho::ARM64_RELOC_PAGE21 => Reloc::Aarch64AdrPrelPgHi21,
                    macho::ARM64_RELOC_PAGEOFF12 => Reloc::Aarch64AddAbsLo12Nc,
                    macho::ARM64_RELOC_GOT_LOAD_PAGE21 => Reloc::Aarch64AdrGotPage21,
                    macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12 => Reloc::Aarch64Ld64GotLo12Nc,
                    macho::ARM64_RELOC_TLVP_LOAD_PAGE21 => Reloc::MachOAarch64TlsAdrPage21,
                    macho::ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => Reloc::MachOAarch64TlsAdrPageOff12,
                    other => return Err(Unhandled::Relocation(other.into())),
                },
                RelocationFlags::Elf { r_type } => match r_type {
                    // A jump out of the function is a call that does not come back.
                    elf::R_AARCH64_CALL26 | elf::R_AARCH64_JUMP26 => Reloc::Arm64Call,
                    elf::R_AARCH64_ADR_PREL_PG_HI21 => Reloc::Aarch64AdrPrelPgHi21,
                    elf::R_AARCH64_ADD_ABS_LO12_NC => Reloc::Aarch64AddAbsLo12Nc,
                    elf::R_AARCH64_ADR_GOT_PAGE => Reloc::Aarch64AdrGotPage21,
                    elf::R_AARCH64_LD64_GOT_LO12_NC => Reloc::Aarch64Ld64GotLo12Nc,
                    elf::R_AARCH64_TLSDESC_ADR_PAGE21 => Reloc::Aarch64TlsDescAdrPage21,
                    elf::R_AARCH64_TLSDESC_LD64_LO12 => Reloc::Aarch64TlsDescLd64Lo12,
                    elf::R_AARCH64_TLSDESC_ADD_LO12 => Reloc::Aarch64TlsDescAddLo12,
                    elf::R_AARCH64_TLSDESC_CALL => Reloc::Aarch64TlsDescCall,
                    other => return Err(Unhandled::Relocation(other)),
                },
                _ => return Err(Unhandled::Shape("an object of a format that is not read")),
            };
            let symbol = Self::symbol(&self.file, &relocation)?;
            let (name, addend) = match symbol.section() {
                SymbolSection::Section(target) => match self.placed[&target] {
                    Placement::Blob(id) => {
                        let within =
                            symbol.address() - self.file.section_by_index(target)?.address();
                        (
                            ModuleRelocTarget::user(1, id.as_u32()),
                            relocation.addend() + within as i64,
                        )
                    }
                    Placement::Inline(_) => {
                        return Err(Unhandled::Shape("code that holds an address of itself"));
                    }
                },
                _ => (
                    Self::target(plain(&self.file, symbol.name()?), recorder)?,
                    relocation.addend(),
                ),
            };
            self.relocs.push(ModuleReloc { offset: offset as u32, kind, name, addend });
        }
        Ok(())
    }

    /// What an undefined symbol of the object stands for.
    fn target(name: &str, recorder: &mut Recorder<'_>) -> Result<ModuleRelocTarget, Unhandled> {
        if let Some(symbol) = Symbol::parse(name) {
            return Ok(match symbol {
                Symbol::Func(index) => ModuleRelocTarget::user(0, index),
                Symbol::Data(index) => ModuleRelocTarget::user(1, index),
            });
        }
        let names = default_libcall_names();
        if let Some(libcall) = LibCall::all_libcalls().iter().find(|&&it| names(it) == name) {
            return Ok(ModuleRelocTarget::LibCall(*libcall));
        }
        // A function LLVM calls of its own accord that Cranelift has no name for: a libm
        // rounding function, wide division. Nothing is checked against the signature of an
        // import.
        let id: FuncId = recorder
            .import(name, &recorder.make_signature())
            .map_err(|_| Unhandled::Shape("a library call that cannot be imported"))?;
        Ok(ModuleRelocTarget::user(0, id.as_u32()))
    }

    /// The function's entry of `.eh_frame` as rules, and its entry of `.gcc_except_table`
    /// as it is: the offsets in it are from the start of the function.
    fn unwind(&self) -> Result<Option<FunctionUnwind>, Unhandled> {
        let name = match self.file.format() {
            BinaryFormat::MachO => "__eh_frame",
            _ => ".eh_frame",
        };
        let Some(section) = self.file.section_by_name(name) else { return Ok(None) };
        let eh_frame = EhFrame::new(section.data()?, LittleEndian);
        let bases = BaseAddresses::default().set_eh_frame(0).set_text(0);
        let malformed = |_| Unhandled::Shape("an unwind table that does not parse");
        let mut entries = eh_frame.entries(&bases);
        let mut rows = None;
        while let Some(entry) = entries.next().map_err(malformed)? {
            let CieOrFde::Fde(partial) = entry else { continue };
            let fde = partial
                .parse(|_, bases, offset| eh_frame.cie_from_offset(bases, offset))
                .map_err(malformed)?;
            if rows.is_some() {
                return Err(Unhandled::Shape("more than one unwind table entry"));
            }
            let cie = fde.cie();
            let entry_rules = Self::rules(cie.instructions(&eh_frame, &bases), cie)?;
            let arch = match self.file.architecture() {
                Architecture::Aarch64 => crate::mir::codegen::Arch::Aarch64,
                _ => crate::mir::codegen::Arch::X86_64,
            };
            if !entry_rules
                .iter()
                .copied()
                .eq(FrameRule::on_entry(arch).iter().map(|&rule| (0, rule)))
            {
                return Err(Unhandled::Shape("an unwind table that starts from other rules"));
            }
            rows = Some(FrameRows {
                len: fde.len() as u32,
                rows: Self::rules(fde.instructions(&eh_frame, &bases), cie)?,
            });
        }
        let Some(rows) = rows else { return Ok(None) };
        Ok(Some(FunctionUnwind::from_rows(rows, self.landing_pads()?)))
    }

    /// The function's entry of `.pdata` as the `UNWIND_INFO` of `.xdata` it names, which is
    /// taken as it is but for the handler: the offsets in it are from the start of the
    /// function.
    fn unwind_coff(&self) -> Result<Option<FunctionUnwind>, Unhandled> {
        const MALFORMED: Unhandled = Unhandled::Shape("an unwind table that does not parse");
        /// The first version of `UNWIND_INFO`, without a flag and with those of a handler
        /// that is called both when a handler is looked for and when the frame is unwound.
        const VERSION: u8 = 1;
        const HANDLED: u8 = VERSION | 0b11 << 3;
        let code = self.code;
        // LLVM writes no entry for a function that calls nothing and leaves the stack as it
        // found it, which the unwinder takes a function without one for. It gets the entry
        // that says as much: whoever looks for where a function ends looks in `.pdata`.
        let Some(pdata) = self.file.section_by_name(".pdata") else {
            let leaf = vec![VERSION, 0, 0, 0];
            return Ok(Some(FunctionUnwind::from_unwind_info(code, leaf, seh::Pads::default())));
        };
        let entry = pdata.data()?;
        // Where the function starts, where it ends and where its `UNWIND_INFO` is.
        let &[_, _, _, _, _, _, _, _, a, b, c, d] = entry else {
            return Err(Unhandled::Shape("more than one unwind table entry"));
        };
        let (_, relocation) =
            pdata.relocations().find(|(offset, _)| *offset == 8).ok_or(MALFORMED)?;
        let symbol = Self::symbol(&self.file, &relocation)?;
        let SymbolSection::Section(xdata) = symbol.section() else { return Err(MALFORMED) };
        let xdata = self.file.section_by_index(xdata)?;
        // The field holds what the linker adds the address of the symbol to.
        let start =
            symbol.address() - xdata.address() + u64::from(u32::from_le_bytes([a, b, c, d]));
        let info = xdata.data()?.get(start as usize..).ok_or(MALFORMED)?;
        let &[version_and_flags, _, codes, _, ..] = info else { return Err(MALFORMED) };
        // The codes are two bytes each and an even number of them is there.
        let (frame, rest) = info
            .split_at_checked(4 + usize::from(codes).next_multiple_of(2) * 2)
            .ok_or(MALFORMED)?;
        let pads = match version_and_flags {
            VERSION => seh::Pads::default(),
            // The address of the personality, which is ours to name, and then its table.
            HANDLED => Self::pads_coff(rest.get(4..).ok_or(MALFORMED)?)?,
            // What is left is an entry that goes on from another.
            _ => return Err(Unhandled::Shape("an unwind table in more than one piece")),
        };
        let mut frame = frame.to_vec();
        frame[0] = VERSION;
        Ok(Some(FunctionUnwind::from_unwind_info(code, frame, pads)))
    }

    /// The calls with a landing pad of LLVM's table of them, which is a `.gcc_except_table`
    /// for a personality LLVM does not know.
    fn pads_coff(table: &[u8]) -> Result<seh::Pads, Unhandled> {
        const MALFORMED: Unhandled = Unhandled::Shape("landing pads that do not parse");
        let mut table = EndianSlice::new(table, LittleEndian);
        let mut uleb = |table: &mut EndianSlice<'_, _>| {
            let value = table.read_uleb128().map_err(|_| MALFORMED)?;
            u32::try_from(value).map_err(|_| MALFORMED)
        };
        // Where the pads are counted from, how the types are written and how the calls
        // are: from the start of the function, not at all, and as numbers of any length.
        let header = table.read_u8_array::<[u8; 3]>().map_err(|_| MALFORMED)?;
        if header != [DW_EH_PE_omit.0, DW_EH_PE_omit.0, DW_EH_PE_uleb128.0] {
            return Err(Unhandled::Shape("landing pads that name a type"));
        }
        let len = uleb(&mut table)?;
        let mut calls = table.split(len as usize).map_err(|_| MALFORMED)?;
        let mut pads = seh::Pads::default();
        while !calls.is_empty() {
            let (start, len, pad, _action) =
                (uleb(&mut calls)?, uleb(&mut calls)?, uleb(&mut calls)?, uleb(&mut calls)?);
            if pad != 0 {
                pads.entered(start..start.checked_add(len).ok_or(MALFORMED)?, pad);
            }
        }
        Ok(pads)
    }

    /// The function's table of landing pads as it is: the offsets in it are from the start
    /// of the function.
    fn landing_pads(&self) -> Result<Option<Vec<u8>>, Unhandled> {
        let name = match self.file.format() {
            BinaryFormat::MachO => "__gcc_except_tab",
            _ => ".gcc_except_table",
        };
        match self.file.section_by_name(name) {
            Some(table) if table.relocations().next().is_some() => {
                Err(Unhandled::Shape("landing pads that name a type"))
            }
            Some(table) => Ok(Some(table.data()?.to_vec())),
            None => Ok(None),
        }
    }

    /// The function's entry of `__compact_unwind` as the rules it stands for, and its entry
    /// of `__gcc_except_tab` as it is. The compact entry says how the frame is once it is
    /// set up and not where in the function that is, so the rules are given from the first
    /// instruction on: right wherever the function calls, which is where a panic unwinds.
    fn unwind_macho(&self) -> Result<Option<FunctionUnwind>, Unhandled> {
        const MODE: u32 = 0x0f00_0000;
        const FRAMELESS: u32 = 0x0200_0000;
        const FRAME: u32 = 0x0400_0000;
        const STACK: u32 = 0x00ff_f000;
        /// The pairs of registers a function saves at the top of its frame, in the order
        /// they are there, by their DWARF numbers.
        const PAIRS: [(u32, u16); 9] = [
            (0x001, 19),
            (0x002, 21),
            (0x004, 23),
            (0x008, 25),
            (0x010, 27),
            (0x100, 72),
            (0x200, 74),
            (0x400, 76),
            (0x800, 78),
        ];
        let Some(section) = self.file.section_by_name("__compact_unwind") else {
            return Ok(None);
        };
        let data = section.data()?;
        let &[_, _, _, _, _, _, _, _, l0, l1, l2, l3, e0, e1, e2, e3, ..] = data else {
            return Ok(None);
        };
        if data.len() != 32 {
            return Err(Unhandled::Shape("more than one unwind table entry"));
        }
        let (len, encoding) =
            (u32::from_le_bytes([l0, l1, l2, l3]), u32::from_le_bytes([e0, e1, e2, e3]));
        let mut rows = Vec::new();
        // How far below the frame the saved pairs start.
        let mut at = match encoding & MODE {
            FRAMELESS => {
                let size = ((encoding & STACK) >> 12) * 16;
                if size != 0 {
                    rows.push((0, FrameRule::CfaOffset(size as i32)));
                }
                0
            }
            FRAME => {
                rows.extend([
                    (0, FrameRule::Cfa(29, 16)),
                    (0, FrameRule::Offset(30, -8)),
                    (0, FrameRule::Offset(29, -16)),
                ]);
                -16
            }
            // The frame was beyond the compact form, and has an entry of `__eh_frame`.
            _ => return self.unwind(),
        };
        for (pair, register) in PAIRS {
            if encoding & pair != 0 {
                rows.push((0, FrameRule::Offset(register, at - 8)));
                rows.push((0, FrameRule::Offset(register + 1, at - 16)));
                at -= 16;
            }
        }
        Ok(Some(FunctionUnwind::from_rows(FrameRows { len, rows }, self.landing_pads()?)))
    }

    const UNKNOWN_RULE: Unhandled = Unhandled::Shape("an unwind rule there is no answer for");

    fn rules<R: gimli::Reader>(
        mut instructions: gimli::CallFrameInstructionIter<'_, R>,
        cie: &gimli::CommonInformationEntry<R>,
    ) -> Result<Vec<(u32, FrameRule)>, Unhandled> {
        use gimli::CallFrameInstruction as Cfi;
        let scale = cie.data_alignment_factor();
        let factored =
            |offset: i64| offset.checked_mul(scale).and_then(|it| i32::try_from(it).ok());
        let mut at = 0u32;
        let mut rules = Vec::new();
        while let Some(instruction) = instructions.next().map_err(|_| Self::UNKNOWN_RULE)? {
            let rule = match instruction {
                Cfi::AdvanceLoc { delta } => {
                    at += delta * cie.code_alignment_factor() as u32;
                    continue;
                }
                Cfi::Nop => continue,
                Cfi::DefCfa { register, offset } => FrameRule::Cfa(register.0, offset as i32),
                Cfi::DefCfaSf { register, factored_offset } => {
                    FrameRule::Cfa(register.0, factored(factored_offset).ok_or(Self::UNKNOWN_RULE)?)
                }
                Cfi::DefCfaRegister { register } => FrameRule::CfaRegister(register.0),
                Cfi::DefCfaOffset { offset } => FrameRule::CfaOffset(offset as i32),
                Cfi::DefCfaOffsetSf { factored_offset } => {
                    FrameRule::CfaOffset(factored(factored_offset).ok_or(Self::UNKNOWN_RULE)?)
                }
                Cfi::Offset { register, factored_offset } => FrameRule::Offset(
                    register.0,
                    factored(factored_offset as i64).ok_or(Self::UNKNOWN_RULE)?,
                ),
                Cfi::OffsetExtendedSf { register, factored_offset } => FrameRule::Offset(
                    register.0,
                    factored(factored_offset).ok_or(Self::UNKNOWN_RULE)?,
                ),
                Cfi::Restore { register } => FrameRule::Restore(register.0),
                Cfi::SameValue { register } => FrameRule::SameValue(register.0),
                Cfi::Undefined { register } => FrameRule::Undefined(register.0),
                Cfi::RememberState => FrameRule::RememberState,
                Cfi::RestoreState => FrameRule::RestoreState,
                Cfi::ArgsSize { size } => FrameRule::ArgsSize(size as u32),
                _ => return Err(Self::UNKNOWN_RULE),
            };
            rules.push((at, rule));
        }
        Ok(rules)
    }
}

/// What a relocation of x86-64 code asks to be put in four bytes of it.
#[derive(Clone, Copy)]
enum Field {
    /// How far away a function is, or the stub the linker makes to reach it.
    Call,
    /// How far away something is.
    Near,
    /// How far away the entry of the GOT is that holds an address.
    Got,
    /// How far away the pair is that `__tls_get_addr` is called with.
    TlsGd,
    /// How far into its section something is: a thread-local of a PE image.
    SectionOffset,
}

/// The name of a symbol as the module had it: Mach-O puts an underscore before it.
fn plain<'n>(file: &object::File<'_>, name: &'n str) -> &'n str {
    match file.format() {
        BinaryFormat::MachO => name.strip_prefix('_').unwrap_or(name),
        _ => name,
    }
}

impl From<object::Error> for Unhandled {
    fn from(e: object::Error) -> Self {
        Unhandled::Llvm(format!("object: {e}"))
    }
}
