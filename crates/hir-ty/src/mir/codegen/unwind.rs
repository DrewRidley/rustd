//! Unwind tables (`.eh_frame`) and landing pads (`.gcc_except_table`), without which a panic
//! cannot cross a function we compiled. Ported from rustc's Cranelift backend.

use cranelift_codegen::{
    Context, FinalizedMachExceptionHandler,
    gimli::{
        self, Encoding, Format, RunTimeEndian, SectionId,
        write::{Address, CieId, EhFrame, EndianVec, FrameTable, Result as WriteResult, Writer},
    },
    ir::{AbiParam, Signature, types},
    isa::{TargetIsa, unwind::UnwindInfo},
};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{
    ObjectProduct,
    object::{
        RelocationEncoding, RelocationFlags, RelocationKind,
        write::{Relocation, StandardSection},
    },
};

mod gcc_except_table;

use gcc_except_table::{
    Action, ActionKind, ActionTable, CallSite, CallSiteTable, GccExceptTable, TypeInfoTable,
};

/// Exception tag of a landing pad that runs destructors and resumes unwinding.
pub(super) const CLEANUP: u32 = 0;
/// Exception tag of a landing pad that stops the unwind, for `catch_unwind`.
pub(super) const CATCH: u32 = 1;

const DATA_SYMBOL: usize = 1 << 31;

fn address_for_func(func: FuncId) -> Address {
    Address::Symbol { symbol: func.as_u32() as usize, addend: 0 }
}

fn address_for_data(data: DataId) -> Address {
    Address::Symbol { symbol: data.as_u32() as usize | DATA_SYMBOL, addend: 0 }
}

/// The frame layout and the landing pads of one compiled function, which do not depend on
/// where the function ends up.
#[derive(PartialEq, Eq)]
pub(super) struct FunctionUnwind {
    info: cranelift_codegen::isa::unwind::systemv::UnwindInfo,
    /// The function's `.gcc_except_table` entry.
    landing_pads: Vec<u8>,
}

impl FunctionUnwind {
    pub(super) fn of(isa: &dyn TargetIsa, context: &Context) -> Result<Option<Self>, String> {
        let Some(code) = context.compiled_code() else { return Ok(None) };
        let Some(UnwindInfo::SystemV(info)) =
            code.create_unwind_info(isa).map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };

        let mut table = GccExceptTable {
            call_sites: CallSiteTable(vec![]),
            actions: ActionTable::new(),
            type_info: TypeInfoTable::new(gimli::DW_EH_PE_udata4),
        };
        let catch_type = table.type_info.add(Address::Constant(0));
        let catch_action =
            table.actions.add(Action { kind: ActionKind::Catch(catch_type), next_action: None });
        for call_site in code.buffer.call_sites() {
            let start = u64::from(call_site.ret_addr - 1);
            if call_site.exception_handlers.is_empty() {
                table.call_sites.0.push(CallSite {
                    start,
                    length: 1,
                    landing_pad: 0,
                    action_entry: None,
                });
            }
            for handler in call_site.exception_handlers {
                let &FinalizedMachExceptionHandler::Tag(tag, landing_pad) = handler else {
                    return Err("unexpected exception handler".to_owned());
                };
                table.call_sites.0.push(CallSite {
                    start,
                    length: 1,
                    landing_pad: u64::from(landing_pad),
                    action_entry: (tag.as_u32() == CATCH).then_some(catch_action),
                });
            }
        }
        let encoding = Encoding {
            format: Format::Dwarf32,
            version: 1,
            address_size: isa.frontend_config().pointer_bytes(),
        };
        let mut writer = WriterRelocate::new();
        table.write(&mut writer, encoding).map_err(|e| e.to_string())?;
        debug_assert!(writer.relocs.is_empty());
        Ok(Some(FunctionUnwind { info, landing_pads: writer.writer.into_vec() }))
    }
}

pub(super) struct UnwindContext {
    frame_table: FrameTable,
    cie: Option<CieId>,
}

impl UnwindContext {
    pub(super) fn new(module: &mut dyn Module) -> Result<Self, String> {
        let mut frame_table = FrameTable::default();
        let Some(mut cie) = module.isa().create_systemv_cie() else {
            return Ok(UnwindContext { frame_table, cie: None });
        };
        let pcrel = gimli::DwEhPe(gimli::DW_EH_PE_pcrel.0 | gimli::DW_EH_PE_sdata4.0);
        cie.fde_address_encoding = pcrel;
        cie.lsda_encoding = Some(pcrel);

        let pointer = module.target_config().pointer_type();
        let personality = module
            .declare_function(
                "rust_eh_personality",
                Linkage::Import,
                &Signature {
                    params: vec![
                        AbiParam::new(types::I32),
                        AbiParam::new(types::I32),
                        AbiParam::new(types::I64),
                        AbiParam::new(pointer),
                        AbiParam::new(pointer),
                    ],
                    returns: vec![AbiParam::new(types::I32)],
                    call_conv: module.target_config().default_call_conv,
                },
            )
            .map_err(|e| e.to_string())?;
        // The personality is reached through a pointer, as it can live in another DSO. It
        // must not be zero-initialized: the unwinder cannot handle it being in `.bss`.
        let personality_ref = module
            .declare_data("DW.ref.rust_eh_personality", Linkage::Local, false, false)
            .map_err(|e| e.to_string())?;
        let mut data = DataDescription::new();
        data.define(
            vec![0; usize::from(module.target_config().pointer_bytes())].into_boxed_slice(),
        );
        let func_ref = module.declare_func_in_data(personality, &mut data);
        data.write_function_addr(0, func_ref);
        module.define_data(personality_ref, &data).map_err(|e| e.to_string())?;
        cie.personality = Some((
            gimli::DwEhPe(
                gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_pcrel.0 | gimli::DW_EH_PE_sdata4.0,
            ),
            address_for_data(personality_ref),
        ));

        Ok(UnwindContext { cie: Some(frame_table.add_cie(cie)), frame_table })
    }

    /// Records the frame layout and the landing pads of a compiled function.
    pub(super) fn add_function(
        &mut self,
        module: &mut dyn Module,
        func: FuncId,
        unwind: &FunctionUnwind,
    ) -> Result<(), String> {
        let Some(cie) = self.cie else { return Ok(()) };
        let mut fde = unwind.info.to_fde(address_for_func(func));
        let lsda = module.declare_anonymous_data(false, false).map_err(|e| e.to_string())?;
        let mut data = DataDescription::new();
        data.define(unwind.landing_pads.clone().into_boxed_slice());
        data.set_custom_section(".gcc_except_table");
        module.define_data(lsda, &data).map_err(|e| e.to_string())?;
        fde.lsda = Some(address_for_data(lsda));

        self.frame_table.add_fde(cie, fde);
        Ok(())
    }

    pub(super) fn emit(self, product: &mut ObjectProduct) -> Result<(), String> {
        let mut eh_frame = EhFrame::from(WriterRelocate::new());
        self.frame_table.write_eh_frame(&mut eh_frame).map_err(|e| e.to_string())?;
        if eh_frame.0.writer.slice().is_empty() {
            return Ok(());
        }
        let section = product.object.section_id(StandardSection::EhFrame);
        product.object.section_mut(section).set_data(eh_frame.0.writer.into_vec(), 8);
        for reloc in &eh_frame.0.relocs {
            let symbol = if reloc.symbol & DATA_SYMBOL == 0 {
                product.function_symbol(FuncId::from_u32(reloc.symbol as u32))
            } else {
                product.data_symbol(DataId::from_u32((reloc.symbol & !DATA_SYMBOL) as u32))
            };
            // Local symbols are addressed through their section.
            let (symbol, offset) =
                product.object.symbol_section_and_offset(symbol).unwrap_or((symbol, 0));
            product
                .object
                .add_relocation(
                    section,
                    Relocation {
                        offset: u64::from(reloc.offset),
                        symbol,
                        flags: RelocationFlags::Generic {
                            kind: reloc.kind,
                            encoding: RelocationEncoding::Generic,
                            size: reloc.size * 8,
                        },
                        addend: offset as i64 + reloc.addend,
                    },
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

struct Reloc {
    offset: u32,
    size: u8,
    symbol: usize,
    addend: i64,
    kind: RelocationKind,
}

/// A [`Writer`] that collects the relocations its output needs.
struct WriterRelocate {
    relocs: Vec<Reloc>,
    writer: EndianVec<RunTimeEndian>,
}

impl WriterRelocate {
    fn new() -> Self {
        WriterRelocate { relocs: Vec::new(), writer: EndianVec::new(RunTimeEndian::Little) }
    }
}

impl Writer for WriterRelocate {
    type Endian = RunTimeEndian;

    fn endian(&self) -> Self::Endian {
        self.writer.endian()
    }

    fn len(&self) -> usize {
        self.writer.len()
    }

    fn write(&mut self, bytes: &[u8]) -> WriteResult<()> {
        self.writer.write(bytes)
    }

    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> WriteResult<()> {
        self.writer.write_at(offset, bytes)
    }

    fn write_address(&mut self, address: Address, size: u8) -> WriteResult<()> {
        match address {
            Address::Constant(val) => self.write_udata(val, size),
            Address::Symbol { symbol, addend } => {
                self.relocs.push(Reloc {
                    offset: self.len() as u32,
                    size,
                    symbol,
                    addend,
                    kind: RelocationKind::Absolute,
                });
                self.write_udata(0, size)
            }
        }
    }

    // `.eh_frame` and the exception table never refer to other debug sections.
    fn write_offset(&mut self, _val: usize, _section: SectionId, _size: u8) -> WriteResult<()> {
        Err(gimli::write::Error::InvalidAttributeValue)
    }

    fn write_offset_at(
        &mut self,
        _offset: usize,
        val: usize,
        section: SectionId,
        size: u8,
    ) -> WriteResult<()> {
        self.write_offset(val, section, size)
    }

    fn write_eh_pointer(
        &mut self,
        address: Address,
        eh_pe: gimli::DwEhPe,
        size: u8,
    ) -> WriteResult<()> {
        match address {
            Address::Constant(val) => {
                let val = match eh_pe.application() {
                    gimli::DW_EH_PE_absptr => val,
                    gimli::DW_EH_PE_pcrel => (self.len() as u64).wrapping_sub(val),
                    _ => return Err(gimli::write::Error::UnsupportedPointerEncoding(eh_pe)),
                };
                self.write_eh_pointer_data(val, eh_pe.format(), size)
            }
            Address::Symbol { symbol, addend } => {
                let (kind, size) = match (eh_pe.application(), eh_pe.format()) {
                    (gimli::DW_EH_PE_pcrel, gimli::DW_EH_PE_sdata4) => {
                        (RelocationKind::Relative, 4)
                    }
                    (gimli::DW_EH_PE_pcrel, gimli::DW_EH_PE_sdata8) => {
                        (RelocationKind::Relative, 8)
                    }
                    (gimli::DW_EH_PE_absptr, _) => (RelocationKind::Absolute, size),
                    _ => return Err(gimli::write::Error::UnsupportedPointerEncoding(eh_pe)),
                };
                self.relocs.push(Reloc { offset: self.len() as u32, size, symbol, addend, kind });
                self.write_udata(0, size)
            }
        }
    }
}
