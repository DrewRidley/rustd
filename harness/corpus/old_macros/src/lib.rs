//! Macros written in an edition before 2024, for a 2024 crate to use.

/// A block whose tail borrows temporaries: before 2024 they outlive the block.
#[macro_export]
macro_rules! values {
    ($show:path; $($name:literal = $value:expr),*) => {{
        let prefix = "";
        let _ = prefix;
        &[$(($name, Some(&$show(&$value)))),*]
    }};
}
