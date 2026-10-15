//! Pure-Rust in-process hot-patch runtime injected into `base.o` when `rustd --patch` is used.
//!
//! Compiled by `rustc --crate-type=rlib --emit=obj -O` and merged into `base.o` so any binary
//! linked with `base.o` (such as `surreal`) automatically watches `patch.o` in a background
//! Rust thread, relocates new Mach-O / ELF patch objects in memory, and atomically updates
//! `<sym>$rustd_ptr` jump-table slots without restarting the process.

#![allow(unsafe_op_in_unsafe_fn, clippy::manual_slice_size_calculation)]

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs,
    path::{Path, PathBuf},
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicPtr, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
static RUSTD_HOTPATCH_INIT: extern "C" fn() = rustd_hotpatch_init;

const PROT_READ: c_int = 0x1;
const PROT_WRITE: c_int = 0x2;
const PROT_EXEC: c_int = 0x4;
const MAP_PRIVATE: c_int = 0x0002;
#[cfg(target_os = "macos")]
const MAP_ANON: c_int = 0x1000;
#[cfg(target_os = "linux")]
const MAP_ANON: c_int = 0x0020;

#[cfg(target_os = "macos")]
const RTLD_DEFAULT: *mut c_void = (-2isize) as *mut c_void;
#[cfg(target_os = "linux")]
const RTLD_DEFAULT: *mut c_void = ptr::null_mut();

unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn sys_icache_invalidate(start: *const c_void, len: usize);
}

struct SavedSlot {
    slot: *const AtomicPtr<c_void>,
    orig: *mut c_void,
}

unsafe impl Send for SavedSlot {}
unsafe impl Sync for SavedSlot {}

static SAVED_SLOTS: Mutex<Vec<SavedSlot>> = Mutex::new(Vec::new());

fn record_and_patch(slot: *const AtomicPtr<c_void>, new_fn: *mut c_void) {
    let mut saved = SAVED_SLOTS.lock().unwrap();
    if !saved.iter().any(|s| ptr::eq(s.slot, slot)) {
        let orig = unsafe { (*slot).load(Ordering::Relaxed) };
        saved.push(SavedSlot { slot, orig });
    }
    unsafe {
        (*slot).store(new_fn, Ordering::Release);
    }
}

fn restore_all() -> usize {
    let mut saved = SAVED_SLOTS.lock().unwrap();
    let count = saved.len();
    for s in saved.drain(..) {
        unsafe {
            (*s.slot).store(s.orig, Ordering::Release);
        }
    }
    count
}

fn lookup_symbol(name: &str) -> *mut c_void {
    let Ok(cstr) = CString::new(name) else { return ptr::null_mut() };
    unsafe { dlsym(RTLD_DEFAULT, cstr.as_ptr()) }
}

fn read_u16(buf: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(off..off + 2)?.try_into().ok()?))
}

fn read_u32(buf: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(off..off + 4)?.try_into().ok()?))
}

fn read_u64(buf: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(buf.get(off..off + 8)?.try_into().ok()?))
}

#[cfg(target_os = "macos")]
fn apply_macho_patch(buf: &[u8]) -> Option<usize> {
    if buf.len() < 32 {
        return None;
    }
    let magic = read_u32(buf, 0)?;
    let filetype = read_u32(buf, 12)?;
    let ncmds = read_u32(buf, 16)? as usize;
    if magic != 0xfeedfacf || filetype != 1 {
        return None;
    }

    let mut seg_off = None;
    let mut sym_off = None;
    let mut lc_pos = 32usize;
    for _ in 0..ncmds {
        let cmd = read_u32(buf, lc_pos)?;
        let cmdsize = read_u32(buf, lc_pos + 4)? as usize;
        if cmd == 0x19 {
            seg_off = Some(lc_pos);
        } else if cmd == 0x2 {
            sym_off = Some(lc_pos);
        }
        lc_pos = lc_pos.checked_add(cmdsize)?;
    }

    let Some(seg_pos) = seg_off else { return Some(restore_all()) };
    let Some(sym_pos) = sym_off else { return Some(restore_all()) };
    let vmsize = read_u64(buf, seg_pos + 32)? as usize;
    let nsects = read_u32(buf, seg_pos + 64)? as usize;
    let symoff = read_u32(buf, sym_pos + 8)? as usize;
    let nsyms = read_u32(buf, sym_pos + 12)? as usize;
    let stroff = read_u32(buf, sym_pos + 16)? as usize;
    if nsyms == 0 {
        return Some(restore_all());
    }

    // Check if there are any defined external symbols; if not, restore base functions.
    let mut has_defined_ext = false;
    for i in 0..nsyms {
        let entry = symoff + i * 16;
        let n_type = *buf.get(entry + 4)?;
        if (n_type & 0x0e) == 0x0e && (n_type & 0x01) != 0 {
            has_defined_ext = true;
            break;
        }
    }
    if !has_defined_ext {
        return Some(restore_all());
    }

    let seg_vmsize = (vmsize + 15) & !15;
    let got_size = nsyms * 8;
    let alloc_size = ((seg_vmsize + got_size + 16383) & !16383).max(16384);

    let jit = unsafe {
        mmap(ptr::null_mut(), alloc_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0)
    } as *mut u8;
    if jit.is_null() || jit as isize == -1 {
        return None;
    }

    struct Sect {
        addr: usize,
        size: usize,
        offset: usize,
        reloff: usize,
        nreloc: usize,
        flags: u32,
    }
    let mut sects = Vec::with_capacity(nsects);
    for s in 0..nsects {
        let sp = seg_pos + 72 + s * 80;
        let sect = Sect {
            addr: read_u64(buf, sp + 32)? as usize,
            size: read_u64(buf, sp + 40)? as usize,
            offset: read_u32(buf, sp + 48)? as usize,
            reloff: read_u32(buf, sp + 56)? as usize,
            nreloc: read_u32(buf, sp + 60)? as usize,
            flags: read_u32(buf, sp + 64)?,
        };
        if (sect.flags & 0xff) != 1 && sect.size > 0 {
            let src = buf.get(sect.offset..sect.offset + sect.size)?;
            unsafe {
                ptr::copy_nonoverlapping(src.as_ptr(), jit.add(sect.addr), sect.size);
            }
        }
        sects.push(sect);
    }

    let got_base = unsafe { jit.add(seg_vmsize) as *mut u64 };
    for i in 0..nsyms {
        let entry = symoff + i * 16;
        let strx = read_u32(buf, entry)? as usize;
        let n_type = *buf.get(entry + 4)?;
        let n_value = read_u64(buf, entry + 8)? as usize;
        let cstr = CStr::from_bytes_until_nul(buf.get(stroff + strx..)?).ok()?;
        let raw_name = cstr.to_str().ok()?;
        let sym_name = raw_name.strip_prefix('_').unwrap_or(raw_name);
        let kind = n_type & 0x0e;
        if kind == 0x0e {
            unsafe { *got_base.add(i) = jit.add(n_value) as u64 };
        } else if kind == 0x00 {
            let resolved = lookup_symbol(sym_name);
            if resolved.is_null() {
                unsafe { munmap(jit as *mut c_void, alloc_size) };
                return None;
            }
            unsafe { *got_base.add(i) = resolved as u64 };
        }
    }

    for sect in &sects {
        let sec_base = unsafe { jit.add(sect.addr) };
        let mut pending_addend: i64 = 0;
        for r in 0..sect.nreloc {
            let rp = sect.reloff + r * 8;
            let r_address = read_u32(buf, rp)? as usize;
            let r_info = read_u32(buf, rp + 4)?;
            let r_symbolnum = (r_info & 0x00ff_ffff) as usize;
            let r_length = (r_info >> 25) & 0x3;
            let r_extern = ((r_info >> 27) & 0x1) != 0;
            let r_type = (r_info >> 28) & 0xf;

            if r_type == 10 {
                // ARM64_RELOC_ADDEND
                let mut add = r_symbolnum as i64;
                if (add & (1 << 23)) != 0 {
                    add |= !((1i64 << 24) - 1);
                }
                pending_addend = add;
                continue;
            }

            let loc = unsafe { sec_base.add(r_address) };
            let target = if r_extern {
                unsafe { *got_base.add(r_symbolnum) }
            } else {
                unsafe { jit.add(sects.get(r_symbolnum.wrapping_sub(1))?.addr) as u64 }
            };

            match r_type {
                0 => {
                    // ARM64_RELOC_UNSIGNED
                    if r_length == 3 {
                        unsafe {
                            let p = loc as *mut u64;
                            let cur = p.read_unaligned();
                            let val = if r_extern {
                                cur.wrapping_add(target).wrapping_add(pending_addend as u64)
                            } else {
                                cur.wrapping_add(jit as u64).wrapping_add(pending_addend as u64)
                            };
                            p.write_unaligned(val);
                        }
                    }
                }
                3 | 5 => {
                    // PAGE21 | GOT_LOAD_PAGE21
                    let t = if r_type == 5 {
                        unsafe { got_base.add(r_symbolnum) as u64 }
                    } else {
                        target.wrapping_add(pending_addend as u64)
                    };
                    let page_diff =
                        (((t & !0xfff) as i64) - (((loc as u64) & !0xfff) as i64)) >> 12;
                    unsafe {
                        let p = loc as *mut u32;
                        let mut insn = p.read_unaligned();
                        insn = (insn & 0x9f00_001f)
                            | (((page_diff as u32) & 0x3) << 29)
                            | ((((page_diff >> 2) as u32) & 0x7ffff) << 5);
                        p.write_unaligned(insn);
                    }
                }
                4 | 6 => {
                    // PAGEOFF12 | GOT_LOAD_PAGEOFF12
                    let t = if r_type == 6 {
                        unsafe { got_base.add(r_symbolnum) as u64 }
                    } else {
                        target.wrapping_add(pending_addend as u64)
                    };
                    let off = (t & 0xfff) as u32;
                    unsafe {
                        let p = loc as *mut u32;
                        let mut insn = p.read_unaligned();
                        let scale = if (insn & 0x3b00_0000) == 0x3900_0000 { insn >> 30 } else { 0 };
                        insn = (insn & !(0xfff << 10)) | (((off >> scale) & 0xfff) << 10);
                        p.write_unaligned(insn);
                    }
                }
                2 => {
                    // ARM64_RELOC_BRANCH26
                    let t = target.wrapping_add(pending_addend as u64);
                    let diff = ((t as i64) - (loc as u64 as i64)) >> 2;
                    unsafe {
                        let p = loc as *mut u32;
                        let mut insn = p.read_unaligned();
                        insn = (insn & 0xfc00_0000) | ((diff as u32) & 0x03ff_ffff);
                        p.write_unaligned(insn);
                    }
                }
                _ => {
                    unsafe { munmap(jit as *mut c_void, alloc_size) };
                    return None;
                }
            }
            pending_addend = 0;
        }
    }

    unsafe {
        sys_icache_invalidate(jit as *const c_void, alloc_size);
        mprotect(jit as *mut c_void, alloc_size, PROT_READ | PROT_EXEC);
    }

    let mut patched = 0usize;
    for i in 0..nsyms {
        let entry = symoff + i * 16;
        let strx = read_u32(buf, entry)? as usize;
        let n_type = *buf.get(entry + 4)?;
        if (n_type & 0x0e) == 0x0e && (n_type & 0x01) != 0 {
            let cstr = CStr::from_bytes_until_nul(buf.get(stroff + strx..)?).ok()?;
            let raw_name = cstr.to_str().ok()?;
            let sym_name = raw_name.strip_prefix('_').unwrap_or(raw_name);
            let ptr_name = format!("{sym_name}$rustd_ptr");
            let slot = lookup_symbol(&ptr_name) as *const AtomicPtr<c_void>;
            if !slot.is_null() {
                let new_fn = unsafe { *got_base.add(i) as *mut c_void };
                record_and_patch(slot, new_fn);
                patched += 1;
            }
        }
    }
    Some(patched)
}

#[cfg(target_os = "linux")]
fn apply_elf_patch(buf: &[u8]) -> Option<usize> {
    if buf.len() < 64 || &buf[0..4] != b"\x7fELF" {
        return None;
    }
    let e_shoff = read_u64(buf, 40)? as usize;
    let e_shentsize = read_u16(buf, 58)? as usize;
    let e_shnum = read_u16(buf, 60)? as usize;
    if e_shnum == 0 {
        return Some(restore_all());
    }

    let mut sec_offsets = vec![0usize; e_shnum];
    let mut total_size = 0usize;
    let mut symtab_idx = None;
    for i in 0..e_shnum {
        let sh = e_shoff + i * e_shentsize;
        let sh_type = read_u32(buf, sh + 4)?;
        let sh_size = read_u64(buf, sh + 32)? as usize;
        let sh_addralign = (read_u64(buf, sh + 48)? as usize).max(1);
        if sh_type == 2 {
            symtab_idx = Some(i);
        }
        total_size = (total_size + sh_addralign - 1) & !(sh_addralign - 1);
        sec_offsets[i] = total_size;
        total_size += sh_size;
    }

    let Some(sym_sec) = symtab_idx else { return Some(restore_all()) };
    let sym_sh = e_shoff + sym_sec * e_shentsize;
    let sym_off = read_u64(buf, sym_sh + 24)? as usize;
    let sym_size = read_u64(buf, sym_sh + 32)? as usize;
    let str_sec = read_u32(buf, sym_sh + 40)? as usize;
    let str_sh = e_shoff + str_sec * e_shentsize;
    let str_off = read_u64(buf, str_sh + 24)? as usize;
    let nsyms = sym_size / 24;

    let mut has_defined_ext = false;
    for i in 1..nsyms {
        let sp = sym_off + i * 24;
        let st_info = *buf.get(sp + 4)?;
        let st_shndx = read_u16(buf, sp + 6)?;
        if (st_info >> 4) != 0 && st_shndx != 0 && st_shndx < 0xff00 {
            has_defined_ext = true;
            break;
        }
    }
    if !has_defined_ext {
        return Some(restore_all());
    }

    let got_offset = (total_size + 15) & !15;
    let alloc_size = ((got_offset + nsyms * 8 + 4095) & !4095).max(4096);
    let jit = unsafe {
        mmap(ptr::null_mut(), alloc_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0)
    } as *mut u8;
    if jit.is_null() || jit as isize == -1 {
        return None;
    }

    for i in 0..e_shnum {
        let sh = e_shoff + i * e_shentsize;
        let sh_type = read_u32(buf, sh + 4)?;
        let sh_offset = read_u64(buf, sh + 24)? as usize;
        let sh_size = read_u64(buf, sh + 32)? as usize;
        if sh_type != 8 && sh_size > 0 {
            let src = buf.get(sh_offset..sh_offset + sh_size)?;
            unsafe {
                ptr::copy_nonoverlapping(src.as_ptr(), jit.add(sec_offsets[i]), sh_size);
            }
        }
    }

    let got_base = unsafe { jit.add(got_offset) as *mut u64 };
    for i in 1..nsyms {
        let sp = sym_off + i * 24;
        let st_name = read_u32(buf, sp)? as usize;
        let st_shndx = read_u16(buf, sp + 6)? as usize;
        let st_value = read_u64(buf, sp + 8)? as usize;
        if st_shndx != 0 && st_shndx < e_shnum {
            unsafe { *got_base.add(i) = jit.add(sec_offsets[st_shndx] + st_value) as u64 };
        } else if st_shndx == 0 {
            let cstr = CStr::from_bytes_until_nul(buf.get(str_off + st_name..)?).ok()?;
            let sym_name = cstr.to_str().ok()?;
            let resolved = lookup_symbol(sym_name);
            if resolved.is_null() {
                unsafe { munmap(jit as *mut c_void, alloc_size) };
                return None;
            }
            unsafe { *got_base.add(i) = resolved as u64 };
        }
    }

    for i in 0..e_shnum {
        let sh = e_shoff + i * e_shentsize;
        let sh_type = read_u32(buf, sh + 4)?;
        if sh_type != 4 {
            continue;
        }
        let rela_off = read_u64(buf, sh + 24)? as usize;
        let rela_size = read_u64(buf, sh + 32)? as usize;
        let target_sec = read_u32(buf, sh + 44)? as usize;
        let sec_base = unsafe { jit.add(sec_offsets[target_sec]) };
        let nrela = rela_size / 24;
        for r in 0..nrela {
            let rp = rela_off + r * 24;
            let r_offset = read_u64(buf, rp)? as usize;
            let r_info = read_u64(buf, rp + 8)?;
            let r_addend = read_u64(buf, rp + 16)? as i64;
            let r_sym = (r_info >> 32) as usize;
            let r_type = (r_info & 0xffff_ffff) as u32;
            let loc = unsafe { sec_base.add(r_offset) };
            let sym_val = unsafe { *got_base.add(r_sym) };
            match r_type {
                1 => {
                    // R_X86_64_64
                    unsafe {
                        *(loc as *mut u64) = sym_val.wrapping_add(r_addend as u64);
                    }
                }
                2 | 4 => {
                    // R_X86_64_PC32 | R_X86_64_PLT32
                    let val = (sym_val as i64)
                        .wrapping_add(r_addend)
                        .wrapping_sub(loc as u64 as i64);
                    unsafe {
                        *(loc as *mut i32) = val as i32;
                    }
                }
                9 | 41 | 42 => {
                    // R_X86_64_GOTPCREL | GOTPCRELX | REX_GOTPCRELX
                    let got_slot = unsafe { got_base.add(r_sym) as u64 as i64 };
                    let val = got_slot.wrapping_add(r_addend).wrapping_sub(loc as u64 as i64);
                    unsafe {
                        *(loc as *mut i32) = val as i32;
                    }
                }
                _ => {
                    unsafe { munmap(jit as *mut c_void, alloc_size) };
                    return None;
                }
            }
        }
    }

    unsafe {
        mprotect(jit as *mut c_void, alloc_size, PROT_READ | PROT_EXEC);
    }

    let mut patched = 0usize;
    for i in 1..nsyms {
        let sp = sym_off + i * 24;
        let st_name = read_u32(buf, sp)? as usize;
        let st_info = *buf.get(sp + 4)?;
        let st_shndx = read_u16(buf, sp + 6)? as usize;
        if (st_info >> 4) != 0 && st_shndx != 0 && st_shndx < e_shnum {
            let cstr = CStr::from_bytes_until_nul(buf.get(str_off + st_name..)?).ok()?;
            let sym_name = cstr.to_str().ok()?;
            let ptr_name = format!("{sym_name}$rustd_ptr");
            let slot = lookup_symbol(&ptr_name) as *const AtomicPtr<c_void>;
            if !slot.is_null() {
                let new_fn = unsafe { *got_base.add(i) as *mut c_void };
                record_and_patch(slot, new_fn);
                patched += 1;
            }
        }
    }
    Some(patched)
}

fn apply_patch_bytes(bytes: &[u8]) -> Option<usize> {
    #[cfg(target_os = "macos")]
    {
        apply_macho_patch(bytes)
    }
    #[cfg(target_os = "linux")]
    {
        apply_elf_patch(bytes)
    }
}

fn watch_loop(patch_path: PathBuf) {
    let _ = lookup_symbol("main");
    let mut last_mtime: Option<SystemTime> =
        fs::metadata(&patch_path).and_then(|m| m.modified()).ok();
    let quiet = std::env::var_os("RUSTD_QUIET").is_some();
    let ack_path = patch_path.with_extension("ack");

    loop {
        thread::sleep(Duration::from_millis(1));
        let Ok(meta) = fs::metadata(&patch_path) else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if Some(mtime) == last_mtime {
            continue;
        }
        last_mtime = Some(mtime);
        let t0 = Instant::now();
        let Ok(bytes) = fs::read(&patch_path) else { continue };
        if let Some(patched) = apply_patch_bytes(&bytes) {
            let elapsed = t0.elapsed();
            let tmp_ack = patch_path.with_extension(format!("ack.tmp.{}", std::process::id()));
            if fs::write(&tmp_ack, format!("{} {}\n", elapsed.as_micros(), patched)).is_ok() {
                let _ = fs::rename(&tmp_ack, &ack_path);
            }
            if !quiet && patched > 0 {
                eprintln!("[rustd] hot-patched {patched} fn(s) in {elapsed:.1?}");
            }
        }
    }
}

extern "C" fn rustd_hotpatch_init() {
    let path = std::env::var_os("RUSTD_PATCH_PATH")
        .map(PathBuf::from)
        .or_else(|| option_env!("RUSTD_DEFAULT_PATCH_PATH").map(PathBuf::from));
    let Some(patch_path) = path else { return };
    if Path::new(&patch_path).exists()
        && let Ok(bytes) = fs::read(&patch_path)
    {
        let _ = apply_patch_bytes(&bytes);
    }
    let _ = thread::Builder::new()
        .name("rustd-hotpatch".into())
        .spawn(move || watch_loop(patch_path));
}
