//! Pure-Rust in-process hot-patch runtime (`base.rt.o`) when `rustd` builds a binary.
//!
//! Supports both:
//! 1. Jump-table slot patching (`<sym>$rustd_ptr` in `__DATA,__data` / `.data`) when `--crate`
//!    pre-compiles a crate into `base.o`.
//! 2. Zero-config workspace-wide inline branch detouring (`b <veneer>` on `aarch64`,
//!    `jmp rel32 <veneer>` on `x86_64`) on any `rustc` function in `__TEXT,__text` / `.text`
//!    via `VENEER_CAVE_START` / `__rustd_veneer_cave_end` and Copy-on-Write `mach_vm_protect` /
//!    `mprotect`, so `rustd run` and `rustd test` work across all workspace crates without
//!    needing `--crate`.

#![allow(unsafe_op_in_unsafe_fn, clippy::manual_slice_size_calculation)]

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs,
    path::{Path, PathBuf},
    ptr,
    sync::{
        Mutex, OnceLock,
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
pub struct VeneerCave([u32; 524288]);

#[used]
#[unsafe(no_mangle)]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__TEXT,__text"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".text"))]
pub static VENEER_CAVE_START: VeneerCave = VeneerCave([0xd503_201f; 524288]);

#[repr(C, align(16384))]
pub struct DataCave([u64; 32768]);

#[used]
#[unsafe(no_mangle)]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__data"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".data"))]
pub static PATCH_DATA_CAVE: DataCave = DataCave([0; 32768]);

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
    fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void;
    fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int;
    fn __register_frame(fde: *const u8);
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
    #[cfg(target_os = "macos")]
    fn _dyld_image_count() -> u32;
    #[cfg(target_os = "macos")]
    fn _dyld_get_image_header(image_index: u32) -> *const u8;
    #[cfg(target_os = "macos")]
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
}

enum SavedPatch {
    PtrSlot { slot: *const AtomicPtr<c_void>, orig: *mut c_void },
    InlineBytes { addr: *mut u8, orig: [u8; 8], len: usize },
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
fn detour_function_inline(orig_fn: *mut u8, new_fn: usize, veneer_idx: &mut usize) -> bool {
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
            |s| matches!(s, SavedPatch::InlineBytes { addr, .. } if ptr::eq(*addr, orig_fn)),
        );
        if !already {
            let mut orig = [0u8; 8];
            ptr::copy_nonoverlapping(orig_fn, orig.as_mut_ptr(), 4);
            saved.push(SavedPatch::InlineBytes { addr: orig_fn, orig, len: 4 });
        }
        if !make_text_writable(orig_addr, 4) {
            return false;
        }
        let b_insn = 0x1400_0000u32 | ((diff as u32) & 0x03ff_ffff);
        (orig_fn as *mut u32).write(b_insn);
        make_text_executable(orig_addr, 4);
    }
    true
}

#[cfg(target_arch = "x86_64")]
fn detour_function_inline(orig_fn: *mut u8, new_fn: usize, veneer_idx: &mut usize) -> bool {
    let orig_addr = orig_fn as usize;
    let start_cave = VENEER_CAVE_START.0.as_ptr() as usize;
    let end_cave = (((&raw const __rustd_veneer_cave_end) as usize) + 4095) & !4095;

    let slot_offset = *veneer_idx * 16;
    if slot_offset + 16 > 65536 {
        return false;
    }
    let cave_base = if orig_addr.abs_diff(start_cave + slot_offset) < 0x7000_0000 {
        start_cave
    } else if orig_addr.abs_diff(end_cave + slot_offset) < 0x7000_0000 {
        end_cave
    } else {
        return false;
    };
    let veneer_addr = cave_base + slot_offset;
    let rel = (veneer_addr as i64) - ((orig_addr + 5) as i64);
    if !(i32::MIN as i64..=i32::MAX as i64).contains(&rel) {
        return false;
    }
    *veneer_idx += 1;

    unsafe {
        if !make_text_writable(veneer_addr, 16) {
            return false;
        }
        let vp = veneer_addr as *mut u8;
        // jmp *0(%rip) followed by 8-byte absolute target
        let stub: [u8; 6] = [0xff, 0x25, 0x00, 0x00, 0x00, 0x00];
        ptr::copy_nonoverlapping(stub.as_ptr(), vp, 6);
        (vp.add(6) as *mut u64).write_unaligned(new_fn as u64);
        make_text_executable(veneer_addr, 16);

        let mut saved = SAVED_PATCHES.lock().unwrap();
        let already = saved.iter().any(
            |s| matches!(s, SavedPatch::InlineBytes { addr, .. } if ptr::eq(*addr, orig_fn)),
        );
        if !already {
            let mut orig = [0u8; 8];
            ptr::copy_nonoverlapping(orig_fn, orig.as_mut_ptr(), 5);
            saved.push(SavedPatch::InlineBytes { addr: orig_fn, orig, len: 5 });
        }
        if !make_text_writable(orig_addr, 5) {
            return false;
        }
        let rel_bytes = (rel as i32).to_le_bytes();
        let jmp: [u8; 5] = [0xe9, rel_bytes[0], rel_bytes[1], rel_bytes[2], rel_bytes[3]];
        ptr::copy_nonoverlapping(jmp.as_ptr(), orig_fn, 5);
        make_text_executable(orig_addr, 5);
    }
    true
}

fn restore_all() -> usize {
    let mut saved = SAVED_PATCHES.lock().unwrap();
    let count = saved.len();
    for s in saved.drain(..) {
        match s {
            SavedPatch::PtrSlot { slot, orig } => unsafe {
                (*slot).store(orig, Ordering::Release);
            },
            SavedPatch::InlineBytes { addr, orig, len } => unsafe {
                if make_text_writable(addr as usize, len) {
                    ptr::copy_nonoverlapping(orig.as_ptr(), addr, len);
                    make_text_executable(addr as usize, len);
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

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __rustd_sym___rust_u128_mulo(a: u128, b: u128, oflow: *mut i32) -> u128 {
    let (res, ov) = a.overflowing_mul(b);
    unsafe { *oflow = i32::from(ov) };
    res
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __rustd_sym___rust_i128_mulo(a: i128, b: i128, oflow: *mut i32) -> i128 {
    let (res, ov) = a.overflowing_mul(b);
    unsafe { *oflow = i32::from(ov) };
    res
}

#[unsafe(no_mangle)]
pub extern "C" fn __rustd_sym___udivti3(a: u128, b: u128) -> u128 {
    a.wrapping_div(b)
}

#[unsafe(no_mangle)]
pub extern "C" fn __rustd_sym___umodti3(a: u128, b: u128) -> u128 {
    a.wrapping_rem(b)
}

#[repr(C, align(16))]
pub struct StaticInnerSlice([usize; 4]);
#[used]
#[unsafe(no_mangle)]
pub static __rustd_sym_STATIC_INNER_SLICE: StaticInnerSlice =
    StaticInnerSlice([usize::MAX >> 1, usize::MAX >> 1, 0, 0]);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __rustd_sym_memcpy(
    dst: *mut c_void,
    src: *const c_void,
    n: usize,
) -> *mut c_void {
    memcpy(dst, src, n)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __rustd_sym_memmove(
    dst: *mut c_void,
    src: *const c_void,
    n: usize,
) -> *mut c_void {
    memmove(dst, src, n)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __rustd_sym_memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    memset(dst, c, n)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __rustd_sym_memcmp(
    s1: *const c_void,
    s2: *const c_void,
    n: usize,
) -> c_int {
    memcmp(s1, s2, n)
}

static SYM_CACHE: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();

#[cfg(target_os = "macos")]
fn build_sym_cache() -> HashMap<&'static str, usize> {
    let mut map = HashMap::new();
    unsafe {
        let count = _dyld_image_count();
        for idx in 0..count {
            let hdr = _dyld_get_image_header(idx);
            if hdr.is_null() {
                continue;
            }
            let magic = (hdr as *const u32).read_unaligned();
            let filetype = (hdr.add(12) as *const u32).read_unaligned();
            if magic != 0xfeed_facf || filetype != 2 {
                continue;
            }
            let slide = _dyld_get_image_vmaddr_slide(idx);
            let ncmds = (hdr.add(16) as *const u32).read_unaligned() as usize;
            let mut linkedit_base: Option<usize> = None;
            let mut symtab_cmd: Option<(usize, usize, usize, usize)> = None;
            let mut lc = hdr.add(32);
            for _ in 0..ncmds {
                let cmd = (lc as *const u32).read_unaligned();
                let cmdsize = (lc.add(4) as *const u32).read_unaligned() as usize;
                if cmd == 0x19 {
                    let segname = std::slice::from_raw_parts(lc.add(8), 16);
                    if segname.starts_with(b"__LINKEDIT\0") {
                        let vmaddr = (lc.add(24) as *const u64).read_unaligned() as usize;
                        let fileoff = (lc.add(40) as *const u64).read_unaligned() as usize;
                        linkedit_base =
                            Some(((vmaddr as isize) + slide) as usize - fileoff);
                    }
                } else if cmd == 0x02 {
                    let symoff = (lc.add(8) as *const u32).read_unaligned() as usize;
                    let nsyms = (lc.add(12) as *const u32).read_unaligned() as usize;
                    let stroff = (lc.add(16) as *const u32).read_unaligned() as usize;
                    let strsize = (lc.add(20) as *const u32).read_unaligned() as usize;
                    symtab_cmd = Some((symoff, nsyms, stroff, strsize));
                }
                lc = lc.add(cmdsize);
            }
            if let (Some(base), Some((symoff, nsyms, stroff, strsize))) =
                (linkedit_base, symtab_cmd)
            {
                map.reserve(nsyms);
                let symtab = (base + symoff) as *const u8;
                let strtab =
                    std::slice::from_raw_parts((base + stroff) as *const u8, strsize);
                for i in 0..nsyms {
                    let entry = symtab.add(i * 16);
                    let n_type = *entry.add(4);
                    if (n_type & 0x0e) != 0x0e {
                        continue;
                    }
                    let n_value = (entry.add(8) as *const u64).read_unaligned() as usize;
                    if n_value == 0 {
                        continue;
                    }
                    let strx = (entry as *const u32).read_unaligned() as usize;
                    let Some(tail) = strtab.get(strx..) else { continue };
                    let Ok(cstr) = CStr::from_bytes_until_nul(tail) else { continue };
                    let Ok(raw) = cstr.to_str() else { continue };
                    let name = raw.strip_prefix('_').unwrap_or(raw);
                    let addr = ((n_value as isize) + slide) as usize;
                    if (n_type & 0x01) != 0 {
                        map.insert(name, addr);
                    } else {
                        map.entry(name).or_insert(addr);
                    }
                }
            }
            break;
        }
    }
    map
}

#[cfg(target_os = "linux")]
fn build_sym_cache() -> HashMap<&'static str, usize> {
    let mut map = HashMap::new();
    let Ok(buf) = fs::read("/proc/self/exe") else { return map };
    if buf.len() < 64 || &buf[0..4] != b"\x7fELF" {
        return map;
    }
    let Some(e_shoff) = read_u64(&buf, 40).map(|v| v as usize) else { return map };
    let Some(e_shentsize) = read_u16(&buf, 58).map(|v| v as usize) else { return map };
    let Some(e_shnum) = read_u16(&buf, 60).map(|v| v as usize) else { return map };
    let mut symtab_idx = None;
    for i in 0..e_shnum {
        let sh = e_shoff + i * e_shentsize;
        if read_u32(&buf, sh + 4) == Some(2) {
            symtab_idx = Some(i);
            break;
        }
    }
    let Some(sym_sec) = symtab_idx else { return map };
    let sym_sh = e_shoff + sym_sec * e_shentsize;
    let Some(sym_off) = read_u64(&buf, sym_sh + 24).map(|v| v as usize) else { return map };
    let Some(sym_size) = read_u64(&buf, sym_sh + 32).map(|v| v as usize) else { return map };
    let Some(str_sec) = read_u32(&buf, sym_sh + 40).map(|v| v as usize) else { return map };
    let str_sh = e_shoff + str_sec * e_shentsize;
    let Some(str_off) = read_u64(&buf, str_sh + 24).map(|v| v as usize) else { return map };
    let nsyms = sym_size / 24;

    let mut unslid_cave = None;
    for i in 1..nsyms {
        let sp = sym_off + i * 24;
        let Some(st_name) = read_u32(&buf, sp).map(|v| v as usize) else { continue };
        let Some(st_shndx) = read_u16(&buf, sp + 6) else { continue };
        let Some(st_value) = read_u64(&buf, sp + 8).map(|v| v as usize) else { continue };
        if st_shndx == 0 || st_shndx >= 0xff00 || st_value == 0 {
            continue;
        }
        let Some(tail) = buf.get(str_off + st_name..) else { continue };
        let Ok(cstr) = CStr::from_bytes_until_nul(tail) else { continue };
        if cstr.to_bytes() == b"VENEER_CAVE_START" {
            unslid_cave = Some(st_value);
            break;
        }
    }
    let Some(cave_vm) = unslid_cave else { return map };
    let rt_cave = (&raw const VENEER_CAVE_START.0) as usize;
    let slide = (rt_cave as isize) - (cave_vm as isize);

    map.reserve(nsyms);
    for i in 1..nsyms {
        let sp = sym_off + i * 24;
        let Some(st_name) = read_u32(&buf, sp).map(|v| v as usize) else { continue };
        let Some(&st_info) = buf.get(sp + 4) else { continue };
        let Some(st_shndx) = read_u16(&buf, sp + 6) else { continue };
        let Some(st_value) = read_u64(&buf, sp + 8).map(|v| v as usize) else { continue };
        if st_shndx == 0 || st_shndx >= 0xff00 || st_value == 0 {
            continue;
        }
        let Some(tail) = buf.get(str_off + st_name..) else { continue };
        let Ok(cstr) = CStr::from_bytes_until_nul(tail) else { continue };
        let Ok(raw) = cstr.to_str() else { continue };
        if raw.is_empty() {
            continue;
        }
        let leaked: &'static str = Box::leak(raw.to_owned().into_boxed_str());
        let addr = ((st_value as isize) + slide) as usize;
        if (st_info >> 4) != 0 {
            map.insert(leaked, addr);
        } else {
            map.entry(leaked).or_insert(addr);
        }
    }
    map
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn build_sym_cache() -> HashMap<&'static str, usize> {
    HashMap::new()
}

static DLSYM_CACHE: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);
static NEW_TEST_FNS: [AtomicPtr<c_void>; 16] = [const { AtomicPtr::new(ptr::null_mut()) }; 16];
static NEW_TESTS: Mutex<Vec<(&'static str, usize)>> = Mutex::new(Vec::new());
static ORIG_TEST_MAIN_STATIC: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());

macro_rules! def_test_runner {
    ($name:ident, $idx:expr) => {
        fn $name() -> Result<(), String> {
            let p = NEW_TEST_FNS[$idx].load(Ordering::Acquire);
            if !p.is_null() {
                let f: extern "C" fn() = unsafe { std::mem::transmute(p) };
                f();
            }
            Ok(())
        }
    };
}
def_test_runner!(rustd_new_test_0, 0);
def_test_runner!(rustd_new_test_1, 1);
def_test_runner!(rustd_new_test_2, 2);
def_test_runner!(rustd_new_test_3, 3);
def_test_runner!(rustd_new_test_4, 4);
def_test_runner!(rustd_new_test_5, 5);
def_test_runner!(rustd_new_test_6, 6);
def_test_runner!(rustd_new_test_7, 7);
def_test_runner!(rustd_new_test_8, 8);
def_test_runner!(rustd_new_test_9, 9);
def_test_runner!(rustd_new_test_10, 10);
def_test_runner!(rustd_new_test_11, 11);
def_test_runner!(rustd_new_test_12, 12);
def_test_runner!(rustd_new_test_13, 13);
def_test_runner!(rustd_new_test_14, 14);
def_test_runner!(rustd_new_test_15, 15);

fn get_test_runner(idx: usize) -> Option<fn() -> Result<(), String>> {
    match idx {
        0 => Some(rustd_new_test_0),
        1 => Some(rustd_new_test_1),
        2 => Some(rustd_new_test_2),
        3 => Some(rustd_new_test_3),
        4 => Some(rustd_new_test_4),
        5 => Some(rustd_new_test_5),
        6 => Some(rustd_new_test_6),
        7 => Some(rustd_new_test_7),
        8 => Some(rustd_new_test_8),
        9 => Some(rustd_new_test_9),
        10 => Some(rustd_new_test_10),
        11 => Some(rustd_new_test_11),
        12 => Some(rustd_new_test_12),
        13 => Some(rustd_new_test_13),
        14 => Some(rustd_new_test_14),
        15 => Some(rustd_new_test_15),
        _ => None,
    }
}

extern "C" fn rustd_test_main_static(tests_ptr: *const *const u64, tests_len: usize) {
    let orig_addr = ORIG_TEST_MAIN_STATIC.load(Ordering::Acquire);
    {
        let mut saved = SAVED_PATCHES.lock().unwrap();
        for s in saved.iter() {
            if let SavedPatch::InlineBytes { addr, orig, len } = s
                && ptr::eq(*addr, orig_addr)
            {
                unsafe {
                    if make_text_writable(*addr as usize, *len) {
                        ptr::copy_nonoverlapping(orig.as_ptr(), *addr, *len);
                        make_text_executable(*addr as usize, *len);
                    }
                }
            }
        }
        saved.retain(|s| !matches!(s, SavedPatch::InlineBytes { addr, .. } if ptr::eq(*addr, orig_addr)));
    }
    let mut extended: Vec<*const u64> = Vec::with_capacity(tests_len + 8);
    if !tests_ptr.is_null() && tests_len > 0 {
        for i in 0..tests_len {
            extended.push(unsafe { *tests_ptr.add(i) });
        }
        let template = unsafe { *tests_ptr };
        if !template.is_null()
            && let Ok(new_tests) = NEW_TESTS.lock()
        {
            for &(name, idx) in new_tests.iter() {
                if let Some(runner) = get_test_runner(idx) {
                    let mut words = [0u64; 19];
                    unsafe {
                        ptr::copy_nonoverlapping(template, words.as_mut_ptr(), 19);
                    }
                    words[4] = name.as_ptr() as u64;
                    words[5] = name.len() as u64;
                    words[6] = 0;
                    words[17] = runner as usize as u64;
                    let leaked = Box::leak(Box::new(words));
                    extended.push(leaked.as_ptr());
                }
            }
        }
    }
    let real_fn: extern "C" fn(*const *const u64, usize) =
        unsafe { std::mem::transmute(orig_addr) };
    real_fn(extended.as_ptr(), extended.len());
}

fn register_new_test(encoded_name: &str, new_fn: *mut c_void, veneer_idx: &mut usize) -> bool {
    let Ok(mut list) = NEW_TESTS.lock() else { return false };
    let decoded = encoded_name.replace("__", "::");
    if let Some(&(_, existing_idx)) = list.iter().find(|&&(n, _)| n == decoded) {
        NEW_TEST_FNS[existing_idx].store(new_fn, Ordering::Release);
        return true;
    }
    let idx = list.len();
    if idx >= 16 {
        return false;
    }
    NEW_TEST_FNS[idx].store(new_fn, Ordering::Release);
    let leaked_name: &'static str = Box::leak(decoded.into_boxed_str());
    list.push((leaked_name, idx));
    drop(list);

    if ORIG_TEST_MAIN_STATIC.load(Ordering::Acquire).is_null() {
        let cache = SYM_CACHE.get_or_init(build_sym_cache);
        if let Some((&_, &addr)) = cache
            .iter()
            .find(|(k, _)| k.ends_with("4test16test_main_static") || **k == "test_main_static")
        {
            let orig_fn = addr as *mut u8;
            if detour_function_inline(
                orig_fn,
                rustd_test_main_static as *const () as usize,
                veneer_idx,
            ) {
                ORIG_TEST_MAIN_STATIC.store(orig_fn, Ordering::Release);
            }
        }
    }
    true
}

fn lookup_symbol(name: &str) -> *mut c_void {
    match name {
        "memcpy" => return __rustd_sym_memcpy as *mut c_void,
        "memmove" => return __rustd_sym_memmove as *mut c_void,
        "memset" => return __rustd_sym_memset as *mut c_void,
        "memcmp" => return __rustd_sym_memcmp as *mut c_void,
        "__rust_u128_mulo" => return __rustd_sym___rust_u128_mulo as *mut c_void,
        "__rust_i128_mulo" => return __rustd_sym___rust_i128_mulo as *mut c_void,
        "__udivti3" => return __rustd_sym___udivti3 as *mut c_void,
        "__umodti3" => return __rustd_sym___umodti3 as *mut c_void,
        "__fixdfti" => return fallback_fixdfti as *mut c_void,
        "__fixunsdfti" => return fallback_fixunsdfti as *mut c_void,
        "__fixsfti" => return fallback_fixsfti as *mut c_void,
        "__fixunssfti" => return fallback_fixunssfti as *mut c_void,
        _ => {}
    }
    let cache = SYM_CACHE.get_or_init(build_sym_cache);
    if let Some(&addr) = cache.get(name) {
        return addr as *mut c_void;
    }
    if name.ends_with("$rustd_ptr") {
        return ptr::null_mut();
    }
    if name.ends_with("STATIC_INNER_SLICE") {
        return (&raw const __rustd_sym_STATIC_INNER_SLICE) as *mut c_void;
    }
    if let Ok(guard) = DLSYM_CACHE.lock()
        && let Some(map) = &*guard
        && let Some(&addr) = map.get(name)
    {
        return addr as *mut c_void;
    }
    let Ok(cstr) = CString::new(name) else { return ptr::null_mut() };
    let p = unsafe { dlsym(RTLD_DEFAULT, cstr.as_ptr()) };
    if let Ok(mut guard) = DLSYM_CACHE.lock() {
        guard.get_or_insert_with(HashMap::new).insert(name.to_owned(), p as usize);
    }
    p
}

#[cfg(target_os = "linux")]
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

    let mut has_defined_ext = false;
    for i in 0..nsyms {
        let entry = symoff + i * 16;
        let n_type = *buf.get(entry + 4)?;
        if (n_type & 0x0e) == 0x0e && (n_type & 0x01) != 0 && (n_type & 0x10) == 0 {
            has_defined_ext = true;
            break;
        }
    }
    if !has_defined_ext {
        return Some(restore_all());
    }

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
        is_eh_frame: bool,
    }
    let mut sects = Vec::with_capacity(nsects);
    for s in 0..nsects {
        let sp = seg_pos + 72 + s * 80;
        let sectname = buf.get(sp..sp + 16).unwrap_or(&[]);
        let is_eh_frame = sectname.starts_with(b"__rustd_eh") || sectname.starts_with(b"__eh_frame");
        let sect = Sect {
            addr: read_u64(buf, sp + 32)? as usize,
            size: read_u64(buf, sp + 40)? as usize,
            offset: read_u32(buf, sp + 48)? as usize,
            reloff: read_u32(buf, sp + 56)? as usize,
            nreloc: read_u32(buf, sp + 60)? as usize,
            flags: read_u32(buf, sp + 64)?,
            is_eh_frame,
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
                3 | 5 | 8 => {
                    let t = if r_type == 5 || r_type == 8 {
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
                4 | 6 | 9 => {
                    let t = if r_type == 6 || r_type == 9 {
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

    let max_exec_end = sects
        .iter()
        .filter(|s| (s.flags & 0x8000_0000) != 0)
        .map(|s| s.addr + s.size)
        .max()
        .unwrap_or(seg_vmsize);
    let rx_size = ((max_exec_end + 16383) & !16383).min(alloc_size);

    unsafe {
        sys_icache_invalidate(jit as *const c_void, rx_size);
        mprotect(jit as *mut c_void, rx_size, PROT_READ | PROT_EXEC);
    }

    for sect in &sects {
        if sect.is_eh_frame {
            let base = unsafe { jit.add(sect.addr) };
            let mut pos = 0usize;
            while pos + 8 <= sect.size {
                let len = unsafe { (base.add(pos) as *const u32).read_unaligned() } as usize;
                if len == 0 || pos + 4 + len > sect.size {
                    break;
                }
                let cie_id = unsafe { (base.add(pos + 4) as *const u32).read_unaligned() };
                if cie_id != 0 {
                    unsafe { __register_frame(base.add(pos)) };
                }
                pos += 4 + len;
            }
        }
    }

    let mut patched = 0usize;
    let mut veneer_idx = 0usize;
    for i in 0..nsyms {
        let entry = symoff + i * 16;
        let strx = read_u32(buf, entry)? as usize;
        let n_type = *buf.get(entry + 4)?;
        if (n_type & 0x0e) == 0x0e && (n_type & 0x01) != 0 && (n_type & 0x10) == 0 {
            let cstr = CStr::from_bytes_until_nul(buf.get(stroff + strx..)?).ok()?;
            let raw_name = cstr.to_str().ok()?;
            let sym_name = raw_name.strip_prefix('_').unwrap_or(raw_name);
            if sym_name.starts_with("rustd::") || sym_name.starts_with("rustd.patch::") {
                continue;
            }
            let new_fn = unsafe { *got_base.add(i) as *mut c_void };
            if let Some(encoded_name) = sym_name.strip_prefix("__rustd_new_test__") {
                if register_new_test(encoded_name, new_fn, &mut veneer_idx) {
                    patched += 1;
                }
                continue;
            }
            let ptr_name = format!("{sym_name}$rustd_ptr");
            let slot = lookup_symbol(&ptr_name) as *const AtomicPtr<c_void>;
            if !slot.is_null() {
                record_and_patch_slot(slot, new_fn);
                patched += 1;
            } else {
                let orig_fn = lookup_symbol(sym_name) as *mut u8;
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
        let st_other = *buf.get(sp + 5)?;
        let st_shndx = read_u16(buf, sp + 6)?;
        if (st_info >> 4) != 0 && (st_other & 0x3) == 0 && st_shndx != 0 && st_shndx < 0xff00 {
            has_defined_ext = true;
            break;
        }
    }
    if !has_defined_ext {
        return Some(restore_all());
    }

    let _ = restore_all();

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
                1 | 257 => {
                    unsafe {
                        (loc as *mut u64).write_unaligned(sym_val.wrapping_add(r_addend as u64));
                    }
                }
                2 | 4 | 261 => {
                    let val = (sym_val as i64)
                        .wrapping_add(r_addend)
                        .wrapping_sub(loc as u64 as i64);
                    unsafe {
                        (loc as *mut i32).write_unaligned(val as i32);
                    }
                }
                9 | 41 | 42 => {
                    let got_slot = unsafe { got_base.add(r_sym) as u64 as i64 };
                    let val = got_slot.wrapping_add(r_addend).wrapping_sub(loc as u64 as i64);
                    unsafe {
                        (loc as *mut i32).write_unaligned(val as i32);
                    }
                }
                282 | 283 => {
                    let t = sym_val.wrapping_add(r_addend as u64);
                    let diff = ((t as i64) - (loc as u64 as i64)) >> 2;
                    unsafe {
                        let p = loc as *mut u32;
                        let mut insn = p.read_unaligned();
                        insn = (insn & 0xfc00_0000) | ((diff as u32) & 0x03ff_ffff);
                        p.write_unaligned(insn);
                    }
                }
                275 | 311 => {
                    let t = if r_type == 311 {
                        unsafe { got_base.add(r_sym) as u64 }
                    } else {
                        sym_val.wrapping_add(r_addend as u64)
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
                277 | 278 | 284 | 285 | 286 | 299 | 312 => {
                    let t = if r_type == 312 {
                        unsafe { got_base.add(r_sym) as u64 }
                    } else {
                        sym_val.wrapping_add(r_addend as u64)
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
    let mut veneer_idx = 0usize;
    for i in 1..nsyms {
        let sp = sym_off + i * 24;
        let st_name = read_u32(buf, sp)? as usize;
        let st_info = *buf.get(sp + 4)?;
        let st_other = *buf.get(sp + 5)?;
        let st_shndx = read_u16(buf, sp + 6)? as usize;
        if (st_info >> 4) != 0 && (st_other & 0x3) == 0 && st_shndx != 0 && st_shndx < e_shnum {
            let cstr = CStr::from_bytes_until_nul(buf.get(str_off + st_name..)?).ok()?;
            let sym_name = cstr.to_str().ok()?;
            if sym_name.starts_with("rustd::") || sym_name.starts_with("rustd.patch::") {
                continue;
            }
            let new_fn = unsafe { *got_base.add(i) as *mut c_void };
            if let Some(encoded_name) = sym_name.strip_prefix("__rustd_new_test__") {
                if register_new_test(encoded_name, new_fn, &mut veneer_idx) {
                    patched += 1;
                }
                continue;
            }
            let ptr_name = format!("{sym_name}$rustd_ptr");
            let slot = lookup_symbol(&ptr_name) as *const AtomicPtr<c_void>;
            if !slot.is_null() {
                record_and_patch_slot(slot, new_fn);
                patched += 1;
            } else {
                let orig_fn = lookup_symbol(sym_name) as *mut u8;
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
    let mut last_mtime: Option<SystemTime> =
        fs::metadata(&patch_path).and_then(|m| m.modified()).ok();
    let _ = SYM_CACHE.get_or_init(build_sym_cache);
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
    std::hint::black_box((
        &raw const PATCH_DATA_CAVE,
        &raw const __rustd_veneer_cave_end,
        &raw const __rustd_sym_STATIC_INNER_SLICE,
        __rustd_sym_memcpy as *const (),
        __rustd_sym_memmove as *const (),
        __rustd_sym_memset as *const (),
        __rustd_sym_memcmp as *const (),
    ));
    unsafe {
        let hdr = (&raw const PATCH_DATA_CAVE.0) as *const u64;
        if hdr.read_volatile() == 0x5255_5354_445f_494e {
            let unslid_cave = hdr.add(1).read_volatile() as usize;
            let count = hdr.add(2).read_volatile() as usize;
            let rt_cave = (&raw const VENEER_CAVE_START.0) as usize;
            let slide = rt_cave.wrapping_sub(unslid_cave);
            if slide != 0 && count > 0 {
                let code_base = rt_cave + 65536;
                if make_text_writable(code_base, 192 * 1024) {
                    for i in 0..count {
                        let off = hdr.add(3 + i).read_volatile() as usize;
                        let p = (code_base + off) as *mut usize;
                        p.write_unaligned(p.read_unaligned().wrapping_add(slide));
                    }
                    make_text_executable(code_base, 192 * 1024);
                }
            }
        }
    }
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
