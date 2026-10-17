//! Whether the profile being built asks for debug information, as cargo decides it: the
//! `CARGO_PROFILE_<NAME>_DEBUG` variable, then `[profile.<name>]` in the manifest at the
//! root of the workspace and the profiles it inherits from, then what cargo's own profiles
//! have.
//!
//! FIXME: a `[profile]` in `.cargo/config.toml` is not read.

use std::path::Path;

use toml::{Table, Value};

/// Whether code built for `profile` in the workspace at `root` is to say where it is from.
pub(crate) fn debug(root: &Path, profile: &str) -> bool {
    // An override for measuring one against the other.
    if let Ok(forced) = std::env::var("RUSTD_DEBUGINFO") {
        return forced != "0";
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).ok();
    let manifest = manifest.and_then(|text| text.parse::<Table>().ok()).unwrap_or_default();
    let profiles = manifest.get("profile").and_then(Value::as_table);
    let mut name = profile.to_owned();
    // A chain of `inherits` ends at one of cargo's profiles; one that does not is a mistake
    // cargo refuses, and is not followed for ever here.
    for _ in 0..16 {
        let variable = format!("CARGO_PROFILE_{}_DEBUG", name.to_uppercase().replace('-', "_"));
        if let Ok(set) = std::env::var(variable) {
            return level(&Value::String(set));
        }
        let table = profiles.and_then(|it| it.get(&name)).and_then(Value::as_table);
        if let Some(set) = table.and_then(|it| it.get("debug")) {
            return level(set);
        }
        match table.and_then(|it| it.get("inherits")).and_then(Value::as_str) {
            Some(parent) => name = parent.to_owned(),
            None => break,
        }
    }
    matches!(name.as_str(), "dev" | "test")
}

/// Whether a `debug` setting asks for anything. Every level that does has line tables,
/// which is what there is to give.
fn level(set: &Value) -> bool {
    match set {
        Value::Boolean(wanted) => *wanted,
        Value::Integer(level) => *level != 0,
        Value::String(level) => !matches!(level.as_str(), "0" | "false" | "none"),
        _ => false,
    }
}
