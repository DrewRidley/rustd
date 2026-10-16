//! Sub-5ms incremental binary linker for `rustd sync` / `rustd build`.
//!
//! Instead of invoking `cc` + `wild` over hundreds of `.rlib` and `.rcgu.o` files on every
//! `rustd sync`, this module clones `<out_bin>.base` via APFS Copy-on-Write (`clonefile` on
//! macOS, `fs::copy` on Linux), relocates `patch.o`'s code and data directly into the
//! pre-allocated `VENEER_CAVE_START` (`__TEXT,__text` / `.text`) and `PATCH_DATA_CAVE`
//! (`__DATA,__data` / `.data`) caves inside the binary, writes inline branch detours (`b` on
//! AArch64, `jmp rel32` on x86_64) or `$rustd_ptr` slots at their exact file offsets, and
//! incrementally updates only the touched 4 KB pages' SHA-256 hashes in `LC_CODE_SIGNATURE`.

use std::{
    collections::BTreeSet,
    ffi::{CStr, CString, c_char, c_int},
    fs::{self, File, OpenOptions},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

use object::{Object, ObjectSymbol};
use rustc_hash::FxHashMap;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> c_int;
}

fn cow_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    let _ = fs::remove_file(dst);
    #[cfg(target_os = "macos")]
    {
        if let (Ok(c_src), Ok(c_dst)) = (
            CString::new(src.as_os_str().as_encoded_bytes()),
            CString::new(dst.as_os_str().as_encoded_bytes()),
        ) && unsafe { clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) } == 0
        {
            return Ok(());
        }
    }
    fs::copy(src, dst).map(|_| ())
}

#[derive(Clone, Copy)]
struct SegmentMap {
    vmaddr: u64,
    fileoff: u64,
    filesize: u64,
}

#[derive(Clone, Copy)]
struct CodeSigInfo {
    dataoff: usize,
    hash_table_off: usize,
    page_size: usize,
    n_code_slots: usize,
}

pub(crate) struct BinaryIndex {
    pub(crate) bin_path: PathBuf,
    pub(crate) base_path: PathBuf,
    is_macho: bool,
    segments: Vec<SegmentMap>,
    symbols: FxHashMap<String, u64>,
    veneer_cave_vmaddr: u64,
    veneer_cave_end_vmaddr: u64,
    data_cave_vmaddr: u64,
    codesig: Option<CodeSigInfo>,
}

fn read_u16_le(buf: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(off..off + 2)?.try_into().ok()?))
}

fn read_u32_le(buf: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(off..off + 4)?.try_into().ok()?))
}

fn read_u32_be(buf: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(buf.get(off..off + 4)?.try_into().ok()?))
}

fn read_u64_le(buf: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(buf.get(off..off + 8)?.try_into().ok()?))
}

impl BinaryIndex {
    pub(crate) fn from_cold_binary(bin_path: &Path) -> Option<Self> {
        let base_path = bin_path.with_extension("base");
        cow_copy(bin_path, &base_path).ok()?;
        let data = fs::read(&base_path).ok()?;
        let obj = object::File::parse(&*data).ok()?;
        let is_macho = obj.format() == object::BinaryFormat::MachO;

        let mut symbols = FxHashMap::default();
        for sym in obj.symbols() {
            if !(sym.section_index().is_some() || sym.is_common()) {
                continue;
            }
            let Ok(raw_name) = sym.name() else { continue };
            let name = if is_macho { raw_name.strip_prefix('_').unwrap_or(raw_name) } else { raw_name };
            if sym.is_global() || name == "VENEER_CAVE_START" || name == "PATCH_DATA_CAVE" || name == "__rustd_veneer_cave_end" {
                symbols.insert(name.to_owned(), sym.address());
            }
        }
        for c_sym in ["memcpy", "memmove", "memset", "memcmp"] {
            if let Some(&addr) = symbols.get(&format!("__rustd_sym_{c_sym}")) {
                symbols.entry(c_sym.to_owned()).or_insert(addr);
            }
        }

        let veneer_cave_vmaddr = *symbols.get("VENEER_CAVE_START")?;
        let veneer_cave_end_vmaddr = symbols.get("__rustd_veneer_cave_end").copied().unwrap_or(0);
        let data_cave_vmaddr = *symbols.get("PATCH_DATA_CAVE")?;

        let mut segments = Vec::new();
        let mut codesig = None;

        if is_macho {
            let ncmds = read_u32_le(&data, 16)? as usize;
            let mut lc_pos = 32usize;
            for _ in 0..ncmds {
                let cmd = read_u32_le(&data, lc_pos)?;
                let cmdsize = read_u32_le(&data, lc_pos + 4)? as usize;
                if cmd == 0x19 {
                    // LC_SEGMENT_64
                    let vmaddr = read_u64_le(&data, lc_pos + 24)?;
                    let fileoff = read_u64_le(&data, lc_pos + 40)?;
                    let filesize = read_u64_le(&data, lc_pos + 48)?;
                    if filesize > 0 {
                        segments.push(SegmentMap { vmaddr, fileoff, filesize });
                    }
                } else if cmd == 0x1d {
                    // LC_CODE_SIGNATURE
                    let dataoff = read_u32_le(&data, lc_pos + 8)? as usize;
                    let count = read_u32_be(&data, dataoff + 8)? as usize;
                    for b in 0..count {
                        let slot_type = read_u32_be(&data, dataoff + 12 + b * 8)?;
                        let blob_rel = read_u32_be(&data, dataoff + 16 + b * 8)? as usize;
                        if slot_type == 0 {
                            let cd_off = dataoff + blob_rel;
                            let cd_magic = read_u32_be(&data, cd_off)?;
                            if cd_magic == 0xfade_0c02 {
                                let hash_offset = read_u32_be(&data, cd_off + 16)? as usize;
                                let n_code_slots = read_u32_be(&data, cd_off + 28)? as usize;
                                let hash_size = *data.get(cd_off + 36)? as usize;
                                let hash_type = *data.get(cd_off + 37)?;
                                let page_exp = *data.get(cd_off + 39)?;
                                if hash_size == 32 && hash_type == 2 && page_exp > 0 {
                                    codesig = Some(CodeSigInfo {
                                        dataoff,
                                        hash_table_off: cd_off + hash_offset,
                                        page_size: 1usize << page_exp,
                                        n_code_slots,
                                    });
                                }
                            }
                        }
                    }
                }
                lc_pos = lc_pos.checked_add(cmdsize)?;
            }
        } else if data.get(0..4) == Some(b"\x7fELF") {
            let e_phoff = read_u64_le(&data, 32)? as usize;
            let e_phentsize = read_u16_le(&data, 54)? as usize;
            let e_phnum = read_u16_le(&data, 56)? as usize;
            for i in 0..e_phnum {
                let ph = e_phoff + i * e_phentsize;
                let p_type = read_u32_le(&data, ph)?;
                if p_type == 1 {
                    // PT_LOAD
                    let fileoff = read_u64_le(&data, ph + 8)?;
                    let vmaddr = read_u64_le(&data, ph + 16)?;
                    let filesize = read_u64_le(&data, ph + 32)?;
                    if filesize > 0 {
                        segments.push(SegmentMap { vmaddr, fileoff, filesize });
                    }
                }
            }
        } else {
            return None;
        }

        Some(Self {
            bin_path: bin_path.to_path_buf(),
            base_path,
            is_macho,
            segments,
            symbols,
            veneer_cave_vmaddr,
            veneer_cave_end_vmaddr,
            data_cave_vmaddr,
            codesig,
        })
    }

    fn vmaddr_to_fileoff(&self, vmaddr: u64, len: u64) -> Option<u64> {
        for seg in &self.segments {
            if vmaddr >= seg.vmaddr && vmaddr.checked_add(len)? <= seg.vmaddr.checked_add(seg.filesize)? {
                return Some(seg.fileoff + (vmaddr - seg.vmaddr));
            }
        }
        None
    }

    pub(crate) fn incremental_link(&self, patch_o: &Path) -> Option<usize> {
        let buf = fs::read(patch_o).ok()?;
        let tmp_path = self.bin_path.with_extension("inc.tmp");
        cow_copy(&self.base_path, &tmp_path).ok()?;
        let file = OpenOptions::new().read(true).write(true).open(&tmp_path).ok()?;
        let mut touched_pages = BTreeSet::new();

        let patched = if self.is_macho {
            self.apply_macho_to_file(&file, &buf, &mut touched_pages)?
        } else {
            self.apply_elf_to_file(&file, &buf, &mut touched_pages)?
        };

        if let Some(cs) = &self.codesig {
            update_codesig_hashes(&file, cs, &touched_pages)?;
        }
        drop(file);
        let _ = fs::remove_file(&self.bin_path);
        fs::rename(&tmp_path, &self.bin_path).ok()?;
        Some(patched)
    }

    fn write_tracked(
        &self,
        file: &File,
        file_off: u64,
        bytes: &[u8],
        touched_pages: &mut BTreeSet<usize>,
    ) -> Option<()> {
        file.write_all_at(bytes, file_off).ok()?;
        if let Some(cs) = &self.codesig {
            let start_page = (file_off as usize) / cs.page_size;
            let end_page = ((file_off as usize) + bytes.len().saturating_sub(1)) / cs.page_size;
            for p in start_page..=end_page {
                if p < cs.n_code_slots {
                    touched_pages.insert(p);
                }
            }
        }
        Some(())
    }

    fn apply_macho_to_file(
        &self,
        file: &File,
        buf: &[u8],
        touched_pages: &mut BTreeSet<usize>,
    ) -> Option<usize> {
        if buf.len() < 32 {
            return None;
        }
        let magic = read_u32_le(buf, 0)?;
        let filetype = read_u32_le(buf, 12)?;
        let ncmds = read_u32_le(buf, 16)? as usize;
        if magic != 0xfeed_facf || filetype != 1 {
            return None;
        }

        let mut seg_off = None;
        let mut sym_off = None;
        let mut lc_pos = 32usize;
        for _ in 0..ncmds {
            let cmd = read_u32_le(buf, lc_pos)?;
            let cmdsize = read_u32_le(buf, lc_pos + 4)? as usize;
            if cmd == 0x19 {
                seg_off = Some(lc_pos);
            } else if cmd == 0x2 {
                sym_off = Some(lc_pos);
            }
            lc_pos = lc_pos.checked_add(cmdsize)?;
        }

        let Some(seg_pos) = seg_off else { return Some(0) };
        let Some(sym_pos) = sym_off else { return Some(0) };
        let vmsize = read_u64_le(buf, seg_pos + 32)? as usize;
        let nsects = read_u32_le(buf, seg_pos + 64)? as usize;
        let symoff = read_u32_le(buf, sym_pos + 8)? as usize;
        let nsyms = read_u32_le(buf, sym_pos + 12)? as usize;
        let stroff = read_u32_le(buf, sym_pos + 16)? as usize;
        if nsyms == 0 {
            return Some(0);
        }

        let seg_vmsize = (vmsize + 15) & !15;
        let got_size = nsyms * 8;
        // Bytes 65536..262144 (192 KB) of VENEER_CAVE_START hold patch sections;
        // PATCH_DATA_CAVE (64 KB) holds the GOT table.
        if seg_vmsize > 192 * 1024 || got_size > 64 * 1024 {
            return None;
        }

        let code_base_vm = self.veneer_cave_vmaddr + 65536;
        let got_base_vm = self.data_cave_vmaddr;

        struct Sect {
            addr: usize,
            size: usize,
            offset: usize,
            reloff: usize,
            nreloc: usize,
            flags: u32,
        }
        let mut sects = Vec::with_capacity(nsects);
        let mut code_buf = vec![0u8; seg_vmsize];
        for s in 0..nsects {
            let sp = seg_pos + 72 + s * 80;
            let sect = Sect {
                addr: read_u64_le(buf, sp + 32)? as usize,
                size: read_u64_le(buf, sp + 40)? as usize,
                offset: read_u32_le(buf, sp + 48)? as usize,
                reloff: read_u32_le(buf, sp + 56)? as usize,
                nreloc: read_u32_le(buf, sp + 60)? as usize,
                flags: read_u32_le(buf, sp + 64)?,
            };
            if (sect.flags & 0xff) != 1 && sect.size > 0 {
                let src = buf.get(sect.offset..sect.offset + sect.size)?;
                code_buf.get_mut(sect.addr..sect.addr + sect.size)?.copy_from_slice(src);
            }
            sects.push(sect);
        }

        let mut got_table = vec![0u64; nsyms];
        for (i, got_slot) in got_table.iter_mut().enumerate() {
            let entry = symoff + i * 16;
            let strx = read_u32_le(buf, entry)? as usize;
            let n_type = *buf.get(entry + 4)?;
            let n_value = read_u64_le(buf, entry + 8)?;
            let cstr = CStr::from_bytes_until_nul(buf.get(stroff + strx..)?).ok()?;
            let raw_name = cstr.to_str().ok()?;
            let sym_name = raw_name.strip_prefix('_').unwrap_or(raw_name);
            let kind = n_type & 0x0e;
            if kind == 0x0e {
                *got_slot = code_base_vm + n_value;
            } else if kind == 0x00 {
                let resolved = self
                    .symbols
                    .get(sym_name)
                    .copied()
                    .or_else(|| {
                        if sym_name.ends_with("STATIC_INNER_SLICE") {
                            self.symbols.get("__rustd_sym_STATIC_INNER_SLICE").copied()
                        } else {
                            None
                        }
                    })?;
                *got_slot = resolved;
            }
        }

        let mut abs_reloc_offs: Vec<u64> = Vec::new();
        for sect in &sects {
            let mut pending_addend: i64 = 0;
            for r in 0..sect.nreloc {
                let rp = sect.reloff + r * 8;
                let r_address = read_u32_le(buf, rp)? as usize;
                let r_info = read_u32_le(buf, rp + 4)?;
                let r_symbolnum = (r_info & 0x00ff_ffff) as usize;
                let r_length = (r_info >> 25) & 0x3;
                let r_extern = ((r_info >> 27) & 0x1) != 0;
                let r_type = (r_info >> 28) & 0xf;

                if r_type == 10 {
                    let mut add = r_symbolnum as i64;
                    if (add & (1 << 23)) != 0 {
                        add |= !((1i64 << 24) - 1);
                    }
                    pending_addend = add;
                    continue;
                }

                let loc_off = sect.addr + r_address;
                let loc_vm = code_base_vm + (loc_off as u64);
                let target = if r_extern {
                    *got_table.get(r_symbolnum)?
                } else {
                    code_base_vm + (sects.get(r_symbolnum.wrapping_sub(1))?.addr as u64)
                };

                match r_type {
                    0 => {
                        if r_length == 3 {
                            let cur = read_u64_le(&code_buf, loc_off)?;
                            let val = if r_extern {
                                cur.wrapping_add(target).wrapping_add(pending_addend as u64)
                            } else {
                                cur.wrapping_add(code_base_vm).wrapping_add(pending_addend as u64)
                            };
                            code_buf
                                .get_mut(loc_off..loc_off + 8)?
                                .copy_from_slice(&val.to_le_bytes());
                            abs_reloc_offs.push(loc_off as u64);
                        }
                    }
                    3 | 5 => {
                        // Relax GOT_LOAD_PAGE21 (5) to PC-relative PAGE21 (3) against `target`.
                        let t = target.wrapping_add(pending_addend as u64);
                        let page_diff = (((t & !0xfff) as i64) - ((loc_vm & !0xfff) as i64)) >> 12;
                        let mut insn = read_u32_le(&code_buf, loc_off)?;
                        insn = (insn & 0x9f00_001f)
                            | (((page_diff as u32) & 0x3) << 29)
                            | ((((page_diff >> 2) as u32) & 0x7ffff) << 5);
                        code_buf
                            .get_mut(loc_off..loc_off + 4)?
                            .copy_from_slice(&insn.to_le_bytes());
                    }
                    4 => {
                        let t = target.wrapping_add(pending_addend as u64);
                        let off = (t & 0xfff) as u32;
                        let mut insn = read_u32_le(&code_buf, loc_off)?;
                        let scale = if (insn & 0x3b00_0000) == 0x3900_0000 { insn >> 30 } else { 0 };
                        insn = (insn & !(0xfff << 10)) | (((off >> scale) & 0xfff) << 10);
                        code_buf
                            .get_mut(loc_off..loc_off + 4)?
                            .copy_from_slice(&insn.to_le_bytes());
                    }
                    6 => {
                        // Relax GOT_LOAD_PAGEOFF12 (6) `ldr xd, [xn, #off]` into `add xd, xn, #off`.
                        let t = target.wrapping_add(pending_addend as u64);
                        let off = (t & 0xfff) as u32;
                        let insn = read_u32_le(&code_buf, loc_off)?;
                        let add_insn = 0x9100_0000u32 | (off << 10) | (insn & 0x3ff);
                        code_buf
                            .get_mut(loc_off..loc_off + 4)?
                            .copy_from_slice(&add_insn.to_le_bytes());
                    }
                    2 => {
                        let t = target.wrapping_add(pending_addend as u64);
                        let diff = ((t as i64) - (loc_vm as i64)) >> 2;
                        if !(-0x0200_0000..0x0200_0000).contains(&diff) {
                            return None;
                        }
                        let mut insn = read_u32_le(&code_buf, loc_off)?;
                        insn = (insn & 0xfc00_0000) | ((diff as u32) & 0x03ff_ffff);
                        code_buf
                            .get_mut(loc_off..loc_off + 4)?
                            .copy_from_slice(&insn.to_le_bytes());
                    }
                    _ => return None,
                }
                pending_addend = 0;
            }
        }

        if !code_buf.is_empty() {
            let code_file_off = self.vmaddr_to_fileoff(code_base_vm, code_buf.len() as u64)?;
            self.write_tracked(file, code_file_off, &code_buf, touched_pages)?;
        }
        let mut header_words = Vec::with_capacity(3 + abs_reloc_offs.len());
        header_words.push(0x5255_5354_445f_494eu64);
        header_words.push(self.veneer_cave_vmaddr);
        header_words.push(abs_reloc_offs.len() as u64);
        header_words.extend_from_slice(&abs_reloc_offs);
        if header_words.len() * 8 > 64 * 1024 {
            return None;
        }
        let mut got_bytes = Vec::with_capacity(header_words.len() * 8);
        for &w in &header_words {
            got_bytes.extend_from_slice(&w.to_le_bytes());
        }
        if !got_bytes.is_empty() {
            let got_file_off = self.vmaddr_to_fileoff(got_base_vm, got_bytes.len() as u64)?;
            self.write_tracked(file, got_file_off, &got_bytes, touched_pages)?;
        }

        let mut patched = 0usize;
        let mut veneer_idx = 0usize;
        for (i, &new_fn_vm) in got_table.iter().enumerate() {
            let entry = symoff + i * 16;
            let strx = read_u32_le(buf, entry)? as usize;
            let n_type = *buf.get(entry + 4)?;
            if (n_type & 0x0e) == 0x0e && (n_type & 0x01) != 0 && (n_type & 0x10) == 0 {
                let cstr = CStr::from_bytes_until_nul(buf.get(stroff + strx..)?).ok()?;
                let raw_name = cstr.to_str().ok()?;
                let sym_name = raw_name.strip_prefix('_').unwrap_or(raw_name);
                if sym_name.starts_with("rustd::") || sym_name.starts_with("rustd.patch::") {
                    continue;
                }
                let ptr_name = format!("{sym_name}$rustd_ptr");
                if let Some(&slot_vm) = self.symbols.get(&ptr_name) {
                    let slot_off = self.vmaddr_to_fileoff(slot_vm, 8)?;
                    self.write_tracked(file, slot_off, &new_fn_vm.to_le_bytes(), touched_pages)?;
                    patched += 1;
                } else if let Some(&orig_vm) = self.symbols.get(sym_name) {
                    let orig_off = self.vmaddr_to_fileoff(orig_vm, 4)?;
                    let direct_diff = ((new_fn_vm as i64) - (orig_vm as i64)) >> 2;
                    if (-0x0200_0000..0x0200_0000).contains(&direct_diff) {
                        let b_insn = 0x1400_0000u32 | ((direct_diff as u32) & 0x03ff_ffff);
                        self.write_tracked(file, orig_off, &b_insn.to_le_bytes(), touched_pages)?;
                        patched += 1;
                    } else {
                        let slot_offset = (veneer_idx * 16) as u64;
                        if slot_offset + 16 > 65536 {
                            return None;
                        }
                        let end_cave = (self.veneer_cave_end_vmaddr + 16383) & !16383;
                        let veneer_vm = if orig_vm.abs_diff(self.veneer_cave_vmaddr + slot_offset)
                            < 120 * 1024 * 1024
                        {
                            self.veneer_cave_vmaddr + slot_offset
                        } else if end_cave != 0
                            && orig_vm.abs_diff(end_cave + slot_offset) < 120 * 1024 * 1024
                        {
                            end_cave + slot_offset
                        } else {
                            return None;
                        };
                        let diff = ((veneer_vm as i64) - (orig_vm as i64)) >> 2;
                        if !(-0x0200_0000..0x0200_0000).contains(&diff) {
                            return None;
                        }
                        veneer_idx += 1;
                        let veneer_off = self.vmaddr_to_fileoff(veneer_vm, 16)?;
                        let mut stub = [0u8; 16];
                        stub[0..4].copy_from_slice(&0x5800_0050u32.to_le_bytes());
                        stub[4..8].copy_from_slice(&0xd61f_0200u32.to_le_bytes());
                        stub[8..16].copy_from_slice(&new_fn_vm.to_le_bytes());
                        self.write_tracked(file, veneer_off, &stub, touched_pages)?;
                        let b_insn = 0x1400_0000u32 | ((diff as u32) & 0x03ff_ffff);
                        self.write_tracked(file, orig_off, &b_insn.to_le_bytes(), touched_pages)?;
                        patched += 1;
                    }
                }
            }
        }
        Some(patched)
    }

    fn apply_elf_to_file(
        &self,
        file: &File,
        buf: &[u8],
        touched_pages: &mut BTreeSet<usize>,
    ) -> Option<usize> {
        if buf.len() < 64 || &buf[0..4] != b"\x7fELF" {
            return None;
        }
        let e_shoff = read_u64_le(buf, 40)? as usize;
        let e_shentsize = read_u16_le(buf, 58)? as usize;
        let e_shnum = read_u16_le(buf, 60)? as usize;
        if e_shnum == 0 {
            return Some(0);
        }

        let mut sec_offsets = vec![0usize; e_shnum];
        let mut total_size = 0usize;
        let mut symtab_idx = None;
        for (i, sec_off) in sec_offsets.iter_mut().enumerate() {
            let sh = e_shoff + i * e_shentsize;
            let sh_type = read_u32_le(buf, sh + 4)?;
            let sh_size = read_u64_le(buf, sh + 32)? as usize;
            let sh_addralign = (read_u64_le(buf, sh + 48)? as usize).max(1);
            if sh_type == 2 {
                symtab_idx = Some(i);
            }
            total_size = (total_size + sh_addralign - 1) & !(sh_addralign - 1);
            *sec_off = total_size;
            total_size += sh_size;
        }

        let Some(sym_sec) = symtab_idx else { return Some(0) };
        let sym_sh = e_shoff + sym_sec * e_shentsize;
        let sym_off = read_u64_le(buf, sym_sh + 24)? as usize;
        let sym_size = read_u64_le(buf, sym_sh + 32)? as usize;
        let str_sec = read_u32_le(buf, sym_sh + 40)? as usize;
        let str_sh = e_shoff + str_sec * e_shentsize;
        let str_off = read_u64_le(buf, str_sh + 24)? as usize;
        let nsyms = sym_size / 24;

        let seg_vmsize = (total_size + 15) & !15;
        let got_size = nsyms * 8;
        if seg_vmsize > 192 * 1024 || got_size > 64 * 1024 {
            return None;
        }

        let code_base_vm = self.veneer_cave_vmaddr + 65536;
        let got_base_vm = self.data_cave_vmaddr;
        let mut code_buf = vec![0u8; seg_vmsize];

        for (i, &sec_off) in sec_offsets.iter().enumerate() {
            let sh = e_shoff + i * e_shentsize;
            let sh_type = read_u32_le(buf, sh + 4)?;
            let sh_offset = read_u64_le(buf, sh + 24)? as usize;
            let sh_size = read_u64_le(buf, sh + 32)? as usize;
            if sh_type != 8 && sh_size > 0 {
                let src = buf.get(sh_offset..sh_offset + sh_size)?;
                code_buf.get_mut(sec_off..sec_off + sh_size)?.copy_from_slice(src);
            }
        }

        let mut got_table = vec![0u64; nsyms];
        for (i, got_slot) in got_table.iter_mut().enumerate().skip(1) {
            let sp = sym_off + i * 24;
            let st_name = read_u32_le(buf, sp)? as usize;
            let st_shndx = read_u16_le(buf, sp + 6)? as usize;
            let st_value = read_u64_le(buf, sp + 8)?;
            if st_shndx != 0 && st_shndx < e_shnum {
                *got_slot = code_base_vm + (sec_offsets[st_shndx] as u64) + st_value;
            } else if st_shndx == 0 {
                let cstr = CStr::from_bytes_until_nul(buf.get(str_off + st_name..)?).ok()?;
                let sym_name = cstr.to_str().ok()?;
                *got_slot = *self.symbols.get(sym_name)?;
            }
        }

        for i in 0..e_shnum {
            let sh = e_shoff + i * e_shentsize;
            let sh_type = read_u32_le(buf, sh + 4)?;
            if sh_type != 4 {
                continue;
            }
            let rela_off = read_u64_le(buf, sh + 24)? as usize;
            let rela_size = read_u64_le(buf, sh + 32)? as usize;
            let target_sec = read_u32_le(buf, sh + 44)? as usize;
            let sec_base_off = sec_offsets[target_sec];
            let nrela = rela_size / 24;
            for r in 0..nrela {
                let rp = rela_off + r * 24;
                let r_offset = read_u64_le(buf, rp)? as usize;
                let r_info = read_u64_le(buf, rp + 8)?;
                let r_addend = read_u64_le(buf, rp + 16)? as i64;
                let r_sym = (r_info >> 32) as usize;
                let r_type = (r_info & 0xffff_ffff) as u32;
                let loc_off = sec_base_off + r_offset;
                let loc_vm = code_base_vm + (loc_off as u64);
                let sym_val = *got_table.get(r_sym)?;
                match r_type {
                    1 => {
                        let val = sym_val.wrapping_add(r_addend as u64);
                        code_buf
                            .get_mut(loc_off..loc_off + 8)?
                            .copy_from_slice(&val.to_le_bytes());
                    }
                    2 | 4 => {
                        let val = (sym_val as i64).wrapping_add(r_addend).wrapping_sub(loc_vm as i64);
                        code_buf
                            .get_mut(loc_off..loc_off + 4)?
                            .copy_from_slice(&(val as i32).to_le_bytes());
                    }
                    9 | 41 | 42 => {
                        let got_slot = (got_base_vm + (r_sym as u64) * 8) as i64;
                        let val = got_slot.wrapping_add(r_addend).wrapping_sub(loc_vm as i64);
                        code_buf
                            .get_mut(loc_off..loc_off + 4)?
                            .copy_from_slice(&(val as i32).to_le_bytes());
                    }
                    _ => return None,
                }
            }
        }

        if !code_buf.is_empty() {
            let code_file_off = self.vmaddr_to_fileoff(code_base_vm, code_buf.len() as u64)?;
            self.write_tracked(file, code_file_off, &code_buf, touched_pages)?;
        }
        let mut got_bytes = Vec::with_capacity(got_table.len() * 8);
        for &g in &got_table {
            got_bytes.extend_from_slice(&g.to_le_bytes());
        }
        if !got_bytes.is_empty() {
            let got_file_off = self.vmaddr_to_fileoff(got_base_vm, got_bytes.len() as u64)?;
            self.write_tracked(file, got_file_off, &got_bytes, touched_pages)?;
        }

        let mut patched = 0usize;
        for (i, &new_fn_vm) in got_table.iter().enumerate().skip(1) {
            let sp = sym_off + i * 24;
            let st_name = read_u32_le(buf, sp)? as usize;
            let st_info = *buf.get(sp + 4)?;
            let st_shndx = read_u16_le(buf, sp + 6)? as usize;
            if (st_info >> 4) != 0 && st_shndx != 0 && st_shndx < e_shnum {
                let cstr = CStr::from_bytes_until_nul(buf.get(str_off + st_name..)?).ok()?;
                let sym_name = cstr.to_str().ok()?;
                if sym_name.starts_with("rustd::") || sym_name.starts_with("rustd.patch::") {
                    continue;
                }
                let ptr_name = format!("{sym_name}$rustd_ptr");
                if let Some(&slot_vm) = self.symbols.get(&ptr_name) {
                    let slot_off = self.vmaddr_to_fileoff(slot_vm, 8)?;
                    self.write_tracked(file, slot_off, &new_fn_vm.to_le_bytes(), touched_pages)?;
                    patched += 1;
                } else if let Some(&orig_vm) = self.symbols.get(sym_name) {
                    let orig_off = self.vmaddr_to_fileoff(orig_vm, 5)?;
                    let rel = (new_fn_vm as i64) - ((orig_vm + 5) as i64);
                    if (i32::MIN as i64..=i32::MAX as i64).contains(&rel) {
                        let rb = (rel as i32).to_le_bytes();
                        let jmp = [0xe9, rb[0], rb[1], rb[2], rb[3]];
                        self.write_tracked(file, orig_off, &jmp, touched_pages)?;
                        patched += 1;
                    }
                }
            }
        }
        Some(patched)
    }
}

fn update_codesig_hashes(
    file: &File,
    cs: &CodeSigInfo,
    touched_pages: &BTreeSet<usize>,
) -> Option<()> {
    let mut page_buf = vec![0u8; cs.page_size];
    for &page_idx in touched_pages {
        let page_off = page_idx * cs.page_size;
        if page_off >= cs.dataoff {
            continue;
        }
        let len = (cs.dataoff - page_off).min(cs.page_size);
        file.read_exact_at(&mut page_buf[..len], page_off as u64).ok()?;
        let digest = sha256(&page_buf[..len]);
        let hash_off = (cs.hash_table_off + page_idx * 32) as u64;
        file.write_all_at(&digest, hash_off).ok()?;
    }
    Some(())
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64) * 8;
    let mut padded = Vec::with_capacity((data.len() + 72) & !63);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for ( slot, val) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(val);
        }
    }
    let mut out = [0u8; 32];
    for (i, &word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}
