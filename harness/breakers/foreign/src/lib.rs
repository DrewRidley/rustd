//! A crate of declarations alone, which rustc builds no code for.

unsafe extern "C" {
    pub fn abs(value: i32) -> i32;
    #[link_name = "labs"]
    pub fn long_abs(value: i64) -> i64;
}
