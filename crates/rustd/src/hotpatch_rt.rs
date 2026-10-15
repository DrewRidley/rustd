//! Pure-Rust in-process hot-patch runtime (`base.rt.o`) when `rustd` builds a binary.
//!
//! Supports both:
//! 1. Jump-table slot patching (`<sym>$rustd_ptr` in `__DATA,__data`) when `--crate` pre-compiles
//!    a crate into `base.o`.
//! 2. Zero-config workspace-wide 4-byte inline branch detouring (`b <veneer>`) on any `rustc`
//!    function in `__TEXT,__text` via `VENEER_CAVE_START` / `__rustd_veneer_cave_end` and
//!    Copy-on-Write `mach_vm_protect` / `mprotect`, so `rustd run` works across all workspace
//!    crates without needing `--crate`.

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

#[repr(C, align(16384))]
struct VeneerCave([u32; 16384]);

#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__TEXT,__text"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".text"))]
static VENEER_CAVE_START: VeneerCave = VeneerCave([0xd503_201f; 16384]);

unsafe extern "C" {
    static __rustd_veneer_cave_end: u32;
}

const PROT_READ: c_int = 0x1;
const PROT_WRITE: c_int = 0x2;
const PROT_EXEC: c_int = 0x4;
const MAP_PRIVATE: c_int = 0x0002;
#[cfg(target_os = "macos")]
const MAP_ANON: c_int = 0x1000;
#[cfg(target_os = "linux")]
const MAP_ANON: c_int = 0x0020;

#[cfg(target_os = "macos")]
const VM_PROT_READ: c_int = 0x01;
#[cfg(target_os = "macos")]
const VM_PROT_WRITE: c_int = 0x02;
#[cfg(target_os = "macos")]
const VM_PROT_EXECUTE: c_int = 0x04;
#[cfg(target_os = "macos")]
const VM_PROT_COPY: c_int = 0x10;

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
    #[cfg(target_os = "macos")]
    fn mach_task_self() -> u32;
    #[cfg(target_os = "macos")]
    fn mach_vm_protect(
        target_task: u32,
        address: u64,
        size: u64,
        set_maximum: c_int,
        new_protection: c_int,
    ) -> c_int;
}

enum SavedPatch {
    PtrSlot { slot: *const AtomicPtr<c_void>, orig: *mut c_void },
    InlineBranch { addr: *mut u32, orig_insn: u32 },
}

unsafe impl Send for SavedPatch {}
unsafe impl Sync for SavedPatch {}

static SAVED_PATCHES: Mutex<Vec<SavedPatch>> = Mutex::new(Vec::new());

unsafe fn make_text_writable(addr: usize, len: usize) -> bool {
    #[cfg(target_os = "macos")]
    {
        let page = (addr & !0x3fff) as u64;
        let end = ((addr + len + 0x3fff) & !0x3fff) as u64;
        let kr =
            mach_vm_protect(mach_task_self(), page, end - page, 0, VM_PROT_READ | VM_PROT_WRITE);
        if kr == 0 {
            return true;
        }
        mach_vm_protect(
            mach_task_self(),
            page,
            end - page,
            0,
            VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY,
        ) == 0
    }
    #[cfg(target_os = "linux")]
    {
        let page = addr & !0xfff;
        let end = (addr + len + 0xfff) & !0xfff;
        mprotect(page as *mut c_void, end - page, PROT_READ | PROT_WRITE | PROT_EXEC) == 0
    }
}

unsafe fn make_text_executable(addr: usize, len: usize) {
    #[cfg(target_os = "macos")]
    {
        sys_icache_invalidate(addr as *const c_void, len);
        let page = (addr & !0x3fff) as u64;
        let end = ((addr + len + 0x3fff) & !0x3fff) as u64;
        let _ = mach_vm_protect(
            mach_task_self(),
            page,
            end - page,
            0,
            VM_PROT_READ | VM_PROT_EXECUTE,
        );
    }
    #[cfg(target_os = "linux")]
    {
        let page = addr & !0xfff;
        let end = (addr + len + 0xfff) & !0xfff;
        let _ = mprotect(page as *mut c_void, end - page, PROT_READ | PROT_EXEC);
    }
}

fn record_and_patch_slot(slot: *const AtomicPtr<c_void>, new_fn: *mut c_void) {
    let mut saved = SAVED_PATCHES.lock().unwrap();
    if !saved.iter().any(|s| matches!(s, SavedPatch::PtrSlot { slot: existing, .. } if ptr::eq(*existing, slot))) {
        let orig = unsafe { (*slot).load(Ordering::Relaxed) };
        saved.push(SavedPatch::PtrSlot { slot, orig });
    }
    unsafe {
        (*slot).store(new_fn, Ordering::Release);
    }
}

#[cfg(target_arch = "aarch64")]
fn detour_function_inline(
    orig_fn: *mut u32,
    new_fn: usize,
    veneer_idx: &mut usize,
) -> bool {
    let orig_addr = orig_fn as usize;
    let start_cave = VENEER_CAVE_START.0.as_ptr() as usize;
    let end_cave = (((&raw const __rustd_veneer_cave_end) as usize) + 16383) & !16383;

    let slot_offset = *veneer_idx * 16;
    if slot_offset + 16 > 65536 {
        return false;
    }
    let cave_base = if orig_addr.abs_diff(start_cave + slot_offset) < 120 * 1024 * 1024 {
        start_cave
    } else if orig_addr.abs_diff(end_cave + slot_offset) < 120 * 1024 * 1024 {
        end_cave
    } else {
        return false;
    };
    let veneer_addr = cave_base + slot_offset;
    let diff = ((veneer_addr as i64) - (orig_addr as i64)) >> 2;
    if !(-0x0200_0000..0x0200_0000).contains(&diff) {
        return false;
    }
    *veneer_idx += 1;

    unsafe {
        if !make_text_writable(veneer_addr, 16) {
            return false;
        }
        let vp = veneer_addr as *mut u32;
        vp.write(0x5800_0050); // ldr x16, #8
        vp.add(1).write(0xd61f_0200); // br x16
        (vp.add(2) as *mut u64).write_unaligned(new_fn as u64);
        make_text_executable(veneer_addr, 16);

        let mut saved = SAVED_PATCHES.lock().unwrap();
        let already = saved.iter().any(
            |s| matches!(s, SavedPatch::InlineBranch { addr, .. } if ptr::eq(*addr, orig_fn)),
        );
        let orig_insn = orig_fn.read();
        if !already {
            saved.push(SavedPatch::InlineBranch { addr: orig_fn, orig_insn });
        }
        if !make_text_writable(orig_addr, 4) {
            return false;
        }
        let b_insn = 0x1400_0000u32 | ((diff as u32) & 0x03ff_ffff);
        orig_fn.write(b_insn);
        make_text_executable(orig_addr, 4);
    }
    true
}

#[cfg(not(target_arch = "aarch64"))]
fn detour_function_inline(
    _orig_fn: *mut u32,
    _new_fn: usize,
    _veneer_idx: &mut usize,
) -> bool {
    false
}

fn restore_all() -> usize {
    let mut saved = SAVED_PATCHES.lock().unwrap();
    let count = saved.len();
    for s in saved.drain(..) {
        match s {
            SavedPatch::PtrSlot { slot, orig } => unsafe {
                (*slot).store(orig, Ordering::Release);
            },
            SavedPatch::InlineBranch { addr, orig_insn } => unsafe {
                if make_text_writable(addr as usize, 4) {
                    addr.write(orig_insn);
                    make_text_executable(addr as usize, 4);
                }
            },
        }
    }
    count
}

extern "C" fn fallback_fixdfti(x: f64) -> i128 {
    x as i128
}

extern "C" fn fallback_fixunsdfti(x: f64) -> u128 {
    x as u128
}

extern "C" fn fallback_fixsfti(x: f32) -> i128 {
    x as i128
}

extern "C" fn fallback_fixunssfti(x: f32) -> u128 {
    x as u128
}

#[repr(C, align(16))]
struct StaticInnerSlice([usize; 4]);
static FALLBACK_STATIC_INNER_SLICE: StaticInnerSlice =
    StaticInnerSlice([usize::MAX >> 1, usize::MAX >> 1, 0, 0]);

fn lookup_symbol(name: &str) -> *mut c_void {
    let Ok(cstr) = CString::new(name) else { return ptr::null_mut() };
    let p = unsafe { dlsym(RTLD_DEFAULT, cstr.as_ptr()) };
    if !p.is_null() {
        return p;
    }
    if name.ends_with("STATIC_INNER_SLICE") {
        return (&raw const FALLBACK_STATIC_INNER_SLICE) as *mut c_void;
    }
    match name {
        "__fixdfti" => fallback_fixdfti as *mut c_void,
        "__fixunsdfti" => fallback_fixunsdfti as *mut c_void,
        "__fixsfti" => fallback_fixsfti as *mut c_void,
        "__fixunssfti" => fallback_fixunssfti as *mut c_void,
        _ => ptr::null_mut(),
    }
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

    // Restore any previous inline/slot patches before applying the current patch object.
    let _ = restore_all();

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
    let mut veneer_idx = 0usize;
    for i in 0..nsyms {
        let entry = symoff + i * 16;
        let strx = read_u32(buf, entry)? as usize;
        let n_type = *buf.get(entry + 4)?;
        if (n_type & 0x0e) == 0x0e && (n_type & 0x01) != 0 {
            let cstr = CStr::from_bytes_until_nul(buf.get(stroff + strx..)?).ok()?;
            let raw_name = cstr.to_str().ok()?;
            let sym_name = raw_name.strip_prefix('_').unwrap_or(raw_name);
            let new_fn = unsafe { *got_base.add(i) as *mut c_void };
            let ptr_name = format!("{sym_name}$rustd_ptr");
            let slot = lookup_symbol(&ptr_name) as *const AtomicPtr<c_void>;
            if !slot.is_null() {
                record_and_patch_slot(slot, new_fn);
                patched += 1;
            } else {
                let orig_fn = lookup_symbol(sym_name) as *mut u32;
                if !orig_fn.is_null()
                    && detour_function_inline(orig_fn, new_fn as usize, &mut veneer_idx)
                {
                    patched += 1;
                }
            }
        }
    }
    Some(patched)
}

#[cfg(target_os = "linux")]
fn apply_elf_patch(_buf: &[u8]) -> Option<usize> {
    None
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
