//! DWARF for the functions of an object: which line every instruction is of (`.debug_line`)
//! and what every function is called and where it is declared (`.debug_info`). A backtrace
//! and a debugger read the same tables rustc writes for its own code, so a frame of ours
//! reads like one of rustc's.
//!
//! A compiled function knows its statements by their place in the body ([`LineRow`]), which
//! an edit above the function leaves alone. The lines are looked up when the function is
//! stitched into an object ([`FunctionDebug`]), and all functions of the object share one
//! compilation unit ([`DebugContext`]).
//!
//! A variable is told of when it is kept in one slot of the frame for as long as the function
//! runs ([`VariableDebug`]), by where the slot is from the frame pointer.

mod types;

pub(super) use types::Types;

use cranelift_codegen::{
    binemit::CodeOffset,
    gimli::{
        self, DW_AT_byte_size, DW_AT_comp_dir, DW_AT_count, DW_AT_data_member_location,
        DW_AT_decl_file, DW_AT_decl_line, DW_AT_encoding, DW_AT_frame_base, DW_AT_high_pc,
        DW_AT_language, DW_AT_linkage_name, DW_AT_location, DW_AT_low_pc, DW_AT_name,
        DW_AT_producer, DW_AT_ranges, DW_AT_stmt_list, DW_AT_type, DW_TAG_array_type,
        DW_TAG_base_type, DW_TAG_formal_parameter, DW_TAG_member, DW_TAG_pointer_type,
        DW_TAG_structure_type, DW_TAG_subprogram, DW_TAG_subrange_type, DW_TAG_variable, DwAte,
        Encoding, Format, LineEncoding, Register, RunTimeEndian, SectionId,
        write::{
            Address, AttributeValue, DwarfUnit, EndianVec, Expression, FileId, LineProgram,
            LineString, Range, RangeList, Result as WriteResult, Sections, UnitEntryId, Writer,
        },
    },
    ir,
    isa::TargetIsa,
};
use cranelift_module::FuncId;
use cranelift_object::{
    ObjectProduct,
    object::{
        BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, SectionKind,
        write::{Relocation, SymbolId},
    },
};
use rustc_hash::FxHashMap;
use serde_derive::{Deserialize, Serialize};

/// The statement the code of a function is of from `offset` on, as an index into the
/// statements the function was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LineRow {
    pub(super) offset: CodeOffset,
    pub(super) span: u32,
}

impl LineRow {
    /// The first source location that stands for a statement: 0 is that of none.
    const FIRST: u32 = 1;

    /// The source location of instructions built for the statement of index `span`.
    pub(super) fn source_loc(span: u32) -> ir::SourceLoc {
        ir::SourceLoc::new(span + Self::FIRST)
    }

    fn span_of(loc: ir::SourceLoc) -> Option<u32> {
        super::unwind::seh::unguarded(loc).bits().checked_sub(Self::FIRST)
    }

    /// The rows of a function from the source location its code has from each offset on, in
    /// the order of the offsets. Code of no statement goes with the statement before it.
    pub(super) fn of_ranges(
        ranges: impl IntoIterator<Item = (CodeOffset, ir::SourceLoc)>,
    ) -> Vec<LineRow> {
        let mut rows: Vec<LineRow> = Vec::new();
        for (offset, loc) in ranges {
            let Some(span) = Self::span_of(loc) else { continue };
            if rows.last().is_none_or(|last| last.span != span) {
                rows.push(LineRow { offset, span });
            }
        }
        rows
    }
}

/// A line of a source file, which is an index into the files of a [`FunctionDebug`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Place {
    pub(super) file: u32,
    pub(super) line: u32,
    pub(super) column: u32,
}

/// What a debugger is told of a type: its name, its size, and what it is made of as far as
/// that is told.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) enum DebugType {
    /// A number, a `bool` or a `char`, whose bytes are read as the `DW_ATE_*` of `encoding`.
    Base {
        name: String,
        size: u64,
        encoding: u8,
    },
    /// A pointer of one word. `to` is `None` where what it points to is not told.
    Pointer {
        name: String,
        to: Option<Box<DebugType>>,
    },
    Array {
        of: Box<DebugType>,
        len: u64,
    },
    /// A struct, a tuple, or a pointer of two words. One without fields may have some that
    /// are not told.
    Struct {
        name: String,
        size: u64,
        fields: Vec<Field>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) struct Field {
    pub(super) name: String,
    pub(super) offset: u64,
    pub(super) ty: DebugType,
}

/// A parameter or a `let` of a function, and where its value is from the frame pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct VariableDebug {
    pub(super) name: String,
    pub(super) param: bool,
    pub(super) ty: DebugType,
    pub(super) offset: i64,
}

/// What a debugger is told about one function, with nothing in it that depends on where the
/// function ends up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct FunctionDebug {
    /// The name as it is written.
    pub(super) name: String,
    /// What a backtrace calls the function: its symbol when that is one rustc mangled, and
    /// otherwise its path the way such a symbol demangles.
    pub(super) linkage_name: String,
    /// The files as rustc names them, relative to the checkout when they are of it.
    pub(super) files: Vec<String>,
    pub(super) declared: Option<Place>,
    /// Where the code is from, from each offset on, in the order of the offsets.
    pub(super) rows: Vec<(CodeOffset, Place)>,
    pub(super) variables: Vec<VariableDebug>,
}

impl FunctionDebug {
    pub(super) fn new(name: String, linkage_name: String) -> Self {
        FunctionDebug {
            name,
            linkage_name,
            files: Vec::new(),
            declared: None,
            rows: Vec::new(),
            variables: Vec::new(),
        }
    }

    /// The place of `line` and `column` in the file rustc calls `file`.
    pub(super) fn place(&mut self, file: String, line: u32, column: u32) -> Place {
        let known = self.files.iter().position(|it| *it == file);
        let file = known.unwrap_or_else(|| {
            self.files.push(file);
            self.files.len() - 1
        });
        Place { file: file as u32, line, column }
    }

    /// Adds that the code from `offset` on is of `place`.
    pub(super) fn row(&mut self, offset: CodeOffset, place: Place) {
        if self.rows.last().is_none_or(|last| last.1 != place) {
            self.rows.push((offset, place));
        }
    }
}

/// The compilation unit of an object, which every function of the object is described in.
pub(super) struct DebugContext {
    dwarf: DwarfUnit,
    files: FxHashMap<String, FileId>,
    ranges: Vec<Range>,
    types: FxHashMap<DebugType, UnitEntryId>,
    /// The register a frame is found by, where variables are told of.
    frame: Option<Register>,
}

impl DebugContext {
    /// The unit of an object for `isa` whose files are named from `directory`, or `None`
    /// where the tables are not written yet: only ELF has them.
    pub(super) fn new(isa: &dyn TargetIsa, directory: &str) -> Option<Self> {
        if isa.triple().binary_format != target_lexicon::BinaryFormat::Elf
            || super::Target::current().is_wasm()
        {
            return None;
        }
        let encoding =
            Encoding { format: Format::Dwarf32, version: 4, address_size: isa.pointer_bytes() };
        let mut dwarf = DwarfUnit::new(encoding);
        let directory = directory.as_bytes().to_vec();
        dwarf.unit.line_program = LineProgram::new(
            encoding,
            LineEncoding::default(),
            LineString::String(directory.clone()),
            None,
            LineString::String(b"<rustd>".to_vec()),
            None,
        );
        let producer = dwarf.strings.add(concat!("rustd ", env!("CARGO_PKG_VERSION")));
        let name = dwarf.strings.add("<rustd>");
        let directory = dwarf.strings.add(directory);
        let root = dwarf.unit.get_mut(dwarf.unit.root());
        root.set(DW_AT_producer, AttributeValue::StringRef(producer));
        root.set(DW_AT_language, AttributeValue::Language(gimli::DW_LANG_Rust));
        root.set(DW_AT_name, AttributeValue::StringRef(name));
        root.set(DW_AT_comp_dir, AttributeValue::StringRef(directory));
        root.set(DW_AT_stmt_list, AttributeValue::LineProgramRef);
        root.set(DW_AT_low_pc, AttributeValue::Address(Address::Constant(0)));
        // FIXME: variables are told of on x86-64 alone, by `rbp`.
        let frame = (isa.triple().architecture == target_lexicon::Architecture::X86_64)
            .then_some(gimli::X86_64::RBP);
        Some(DebugContext {
            dwarf,
            files: FxHashMap::default(),
            ranges: Vec::new(),
            types: FxHashMap::default(),
            frame,
        })
    }

    fn file(&mut self, name: &str) -> FileId {
        if let Some(&id) = self.files.get(name) {
            return id;
        }
        let program = &mut self.dwarf.unit.line_program;
        let directory = program.default_directory();
        let id = program.add_file(LineString::String(name.as_bytes().to_vec()), directory, None);
        self.files.insert(name.to_owned(), id);
        id
    }

    /// The entry of `ty`, which every variable of the type shares.
    fn ty(&mut self, ty: &DebugType) -> UnitEntryId {
        if let Some(&entry) = self.types.get(ty) {
            return entry;
        }
        let root = self.dwarf.unit.root();
        let entry = match ty {
            DebugType::Base { name, size, encoding } => {
                let entry = self.named(root, DW_TAG_base_type, name);
                let unit = self.dwarf.unit.get_mut(entry);
                unit.set(DW_AT_byte_size, AttributeValue::Udata(*size));
                unit.set(DW_AT_encoding, AttributeValue::Encoding(DwAte(*encoding)));
                entry
            }
            DebugType::Pointer { name, to } => {
                let to = to.as_ref().map(|to| self.ty(to));
                let entry = self.named(root, DW_TAG_pointer_type, name);
                if let Some(to) = to {
                    self.dwarf.unit.get_mut(entry).set(DW_AT_type, AttributeValue::UnitRef(to));
                }
                entry
            }
            DebugType::Array { of, len } => {
                let of = self.ty(of);
                let unit = &mut self.dwarf.unit;
                let entry = unit.add(root, DW_TAG_array_type);
                unit.get_mut(entry).set(DW_AT_type, AttributeValue::UnitRef(of));
                let range = unit.add(entry, DW_TAG_subrange_type);
                unit.get_mut(range).set(DW_AT_count, AttributeValue::Udata(*len));
                entry
            }
            DebugType::Struct { name, size, fields } => {
                let entry = self.named(root, DW_TAG_structure_type, name);
                self.dwarf.unit.get_mut(entry).set(DW_AT_byte_size, AttributeValue::Udata(*size));
                for field in fields {
                    let ty = self.ty(&field.ty);
                    let member = self.named(entry, DW_TAG_member, &field.name);
                    let member = self.dwarf.unit.get_mut(member);
                    member.set(DW_AT_type, AttributeValue::UnitRef(ty));
                    member.set(DW_AT_data_member_location, AttributeValue::Udata(field.offset));
                }
                entry
            }
        };
        self.types.insert(ty.clone(), entry);
        entry
    }

    fn named(&mut self, parent: UnitEntryId, tag: gimli::DwTag, name: &str) -> UnitEntryId {
        let name = self.dwarf.strings.add(name.as_bytes());
        let entry = self.dwarf.unit.add(parent, tag);
        self.dwarf.unit.get_mut(entry).set(DW_AT_name, AttributeValue::StringRef(name));
        entry
    }

    /// Tells of the variables of the function at `function`.
    fn variables(&mut self, function: UnitEntryId, variables: &[VariableDebug]) {
        let Some(frame) = self.frame.filter(|_| !variables.is_empty()) else { return };
        let mut base = Expression::new();
        base.op_reg(frame);
        self.dwarf.unit.get_mut(function).set(DW_AT_frame_base, AttributeValue::Exprloc(base));
        for variable in variables {
            let ty = self.ty(&variable.ty);
            let tag = if variable.param { DW_TAG_formal_parameter } else { DW_TAG_variable };
            let entry = self.named(function, tag, &variable.name);
            let mut place = Expression::new();
            place.op_fbreg(variable.offset);
            let entry = self.dwarf.unit.get_mut(entry);
            entry.set(DW_AT_type, AttributeValue::UnitRef(ty));
            entry.set(DW_AT_location, AttributeValue::Exprloc(place));
        }
    }

    /// Describes the function `func` of the module, which is `len` bytes of code.
    pub(super) fn add_function(&mut self, func: FuncId, len: u32, debug: &FunctionDebug) {
        if len == 0 {
            return;
        }
        let start = Address::Symbol { symbol: func.as_u32() as usize, addend: 0 };
        let files: Vec<FileId> = debug.files.iter().map(|it| self.file(it)).collect();
        let file = |place: &Place| files.get(place.file as usize).copied();
        self.ranges.push(Range::StartLength { begin: start, length: len.into() });

        let name = self.dwarf.strings.add(debug.name.as_bytes());
        let linkage_name = self.dwarf.strings.add(debug.linkage_name.as_bytes());
        let unit = &mut self.dwarf.unit;
        let function = unit.add(unit.root(), DW_TAG_subprogram);
        let entry = unit.get_mut(function);
        entry.set(DW_AT_name, AttributeValue::StringRef(name));
        entry.set(DW_AT_linkage_name, AttributeValue::StringRef(linkage_name));
        entry.set(DW_AT_low_pc, AttributeValue::Address(start));
        entry.set(DW_AT_high_pc, AttributeValue::Udata(len.into()));
        if let Some(declared) = &debug.declared
            && let Some(file) = file(declared)
        {
            entry.set(DW_AT_decl_file, AttributeValue::FileIndex(Some(file)));
            entry.set(DW_AT_decl_line, AttributeValue::Udata(declared.line.into()));
        }
        self.variables(function, &debug.variables);

        // The code ahead of the first statement sets the frame up, which is of the line the
        // function is declared on, or of the first statement when that line is not known.
        let first = debug.rows.first().map(|it| it.1);
        let entry_place = debug.declared.or(first).filter(|_| first.is_some());
        let lead = entry_place.filter(|_| debug.rows.first().is_some_and(|it| it.0 != 0));
        let rows = lead.map(|place| (0, place)).into_iter().chain(debug.rows.iter().copied());
        let program = &mut self.dwarf.unit.line_program;
        let mut any = false;
        for (offset, place) in rows {
            let Some(file) = file(&place) else { continue };
            if !any {
                program.begin_sequence(Some(start));
                any = true;
            }
            let row = program.row();
            row.address_offset = offset.into();
            row.file = file;
            row.line = place.line.into();
            row.column = place.column.into();
            program.generate_row();
        }
        if any {
            program.end_sequence(len.into());
        }
    }

    /// Writes the sections into the object, when it has a function to describe.
    pub(super) fn emit(mut self, product: &mut ObjectProduct) -> Result<(), String> {
        if self.ranges.is_empty() || product.object.format() != BinaryFormat::Elf {
            return Ok(());
        }
        let ranges = self.dwarf.unit.ranges.add(RangeList(self.ranges));
        let root = self.dwarf.unit.root();
        self.dwarf.unit.get_mut(root).set(DW_AT_ranges, AttributeValue::RangeListRef(ranges));
        let mut sections = Sections::new(SectionWriter::default());
        self.dwarf.write(&mut sections).map_err(|e| e.to_string())?;

        let mut placed = FxHashMap::default();
        sections.for_each(|id, section: &SectionWriter| {
            if !section.bytes.slice().is_empty() {
                let kind = match id {
                    SectionId::DebugStr | SectionId::DebugLineStr => SectionKind::DebugString,
                    _ => SectionKind::Debug,
                };
                let name = id.name().as_bytes().to_vec();
                let placed_at = product.object.add_section(Vec::new(), name, kind);
                product.object.section_mut(placed_at).set_data(section.bytes.slice().to_vec(), 1);
                placed.insert(id, placed_at);
            }
            Ok::<_, String>(())
        })?;
        sections.for_each(|id, section: &SectionWriter| {
            let Some(&at) = placed.get(&id) else { return Ok(()) };
            for reloc in &section.relocs {
                let (symbol, addend) = match reloc.target {
                    Target::Function(func) => function_place(product, func),
                    Target::Section(section) => {
                        let Some(&section) = placed.get(&section) else {
                            return Err(format!("{} points to a section left out", id.name()));
                        };
                        (product.object.section_symbol(section), 0)
                    }
                };
                let flags = RelocationFlags::Generic {
                    kind: RelocationKind::Absolute,
                    encoding: RelocationEncoding::Generic,
                    size: reloc.size * 8,
                };
                product
                    .object
                    .add_relocation(
                        at,
                        Relocation {
                            offset: reloc.offset,
                            symbol,
                            flags,
                            addend: addend + reloc.addend,
                        },
                    )
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })
    }
}

/// The symbol to name a function by and how far past it the function starts. A local
/// function is named through its section, as `.eh_frame` names it.
fn function_place(product: &mut ObjectProduct, func: FuncId) -> (SymbolId, i64) {
    let symbol = product.function_symbol(func);
    match product.object.symbol_section_and_offset(symbol) {
        Some((section, offset)) => (section, offset as i64),
        None => (symbol, 0),
    }
}

/// What a relocation of a debug section points into.
#[derive(Clone, Copy)]
enum Target {
    Function(FuncId),
    Section(SectionId),
}

#[derive(Clone, Copy)]
struct Reloc {
    offset: u64,
    size: u8,
    target: Target,
    addend: i64,
}

/// A [`Writer`] of one debug section that collects the relocations its output needs: the
/// addresses of functions, and the offsets into other debug sections, which a linker moves.
#[derive(Clone)]
struct SectionWriter {
    relocs: Vec<Reloc>,
    bytes: EndianVec<RunTimeEndian>,
}

impl Default for SectionWriter {
    fn default() -> Self {
        SectionWriter { relocs: Vec::new(), bytes: EndianVec::new(RunTimeEndian::Little) }
    }
}

impl Writer for SectionWriter {
    type Endian = RunTimeEndian;

    fn endian(&self) -> Self::Endian {
        self.bytes.endian()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn write(&mut self, bytes: &[u8]) -> WriteResult<()> {
        self.bytes.write(bytes)
    }

    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> WriteResult<()> {
        self.bytes.write_at(offset, bytes)
    }

    fn write_address(&mut self, address: Address, size: u8) -> WriteResult<()> {
        match address {
            Address::Constant(val) => self.write_udata(val, size),
            Address::Symbol { symbol, addend } => {
                self.relocs.push(Reloc {
                    offset: self.len() as u64,
                    size,
                    target: Target::Function(FuncId::from_u32(symbol as u32)),
                    addend,
                });
                self.write_udata(0, size)
            }
        }
    }

    fn write_offset(&mut self, val: usize, section: SectionId, size: u8) -> WriteResult<()> {
        self.relocs.push(Reloc {
            offset: self.len() as u64,
            size,
            target: Target::Section(section),
            addend: val as i64,
        });
        self.write_udata(0, size)
    }

    fn write_offset_at(
        &mut self,
        offset: usize,
        val: usize,
        section: SectionId,
        size: u8,
    ) -> WriteResult<()> {
        self.relocs.push(Reloc {
            offset: offset as u64,
            size,
            target: Target::Section(section),
            addend: val as i64,
        });
        self.write_udata_at(offset, 0, size)
    }
}
