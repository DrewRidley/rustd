pub const SCALE: u32 = 3;

pub struct Shape {
    pub side: u32,
}

#[inline]
pub fn inlined(x: u32) -> u32 {
    x + 1
}

pub fn describe<T: std::fmt::Debug + Default>() -> String {
    format!("{:?} {}", T::default(), std::mem::size_of::<T>())
}
