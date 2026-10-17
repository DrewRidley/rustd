#!/usr/bin/env python3
"""farsplices.py: edits spliced into a binary whose code is further from its end than a branch
reaches.

The program has 160 MB of zeroed static between its code and where a splice adds to it. On
AArch64 a `b` or `bl` reaches 128 MB, so the jump from a function to its replacement is the
long one, and a function shorter than that is linked in full. On x86-64 every step is within reach and is spliced. Run with RUSTD_SPLICE=1;
each step is compared with what `cargo build` of the same source prints, and with how the
daemon is expected to have made the binary.
"""
from difftest import TARGET, host
from includes import follow

AARCH64 = (TARGET or host()).startswith("aarch64")
MAIN = "src/main.rs"
MAIN_TEXT = """use std::ptr::addr_of_mut;

static mut FAR: [u8; 160 << 20] = [0; 160 << 20];

#[inline(never)]
fn nothing() {}

#[inline(never)]
fn long(x: u64) -> u64 {
    let mut total = x;
    for i in 0..10 {
        total = total.wrapping_mul(31).wrapping_add(i);
    }
    total
}

#[inline(never)]
fn near() -> u64 {
    40
}

#[inline(never)]
fn through() -> u64 {
    near() + 1
}

fn main() {
    unsafe { addr_of_mut!(FAR).cast::<u8>().add(159 << 20).write(7) };
    nothing();
    println!("{} {} {}", long(3), through(), unsafe { addr_of_mut!(FAR).cast::<u8>().add(159 << 20).read() });
}
"""
# What a function too short for the long jump is linked by.
SHORT = "full: " if AARCH64 else "spliced"
STEPS = [
    ("nothing edited", [], False),
    ("a function a long jump fits", [(MAIN, "wrapping_mul(31)", "wrapping_mul(33)")], False, "spliced"),
    ("again", [(MAIN, "wrapping_mul(33)", "wrapping_mul(35)")], False, "spliced"),
    ("the caller of one in the executable", [(MAIN, "near() + 1", "near() + 2")], False, "spliced"),
    ("a function shorter than the long jump", [(MAIN, "fn nothing() {}", 'fn nothing() {\n    println!("something");\n}')], False, SHORT),
    ("an edit after that", [(MAIN, "wrapping_mul(35)", "wrapping_mul(37)")], False, "spliced"),
]

if __name__ == "__main__":
    follow([(MAIN, MAIN_TEXT)], STEPS)
