//! `cargo rustd ...`: cargo runs this as `cargo-rustd rustd ...`, and it becomes
//! `rustd cargo ...`, the `rustd` being the one installed next to it.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let rustd = match std::env::current_exe() {
        Ok(this) => this.with_file_name(format!("rustd{}", std::env::consts::EXE_SUFFIX)),
        Err(error) => {
            eprintln!("error: cargo-rustd does not know where it is installed: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Cargo names the subcommand first.
    let args = std::env::args_os().skip(1).skip_while(|it| it == "rustd");
    match Command::new(&rustd).arg("cargo").args(args).status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("error: cargo-rustd could not run {}: {error}", rustd.display());
            ExitCode::FAILURE
        }
    }
}
