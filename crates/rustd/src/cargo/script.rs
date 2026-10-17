//! Build scripts of the workspace. Cargo runs one again when what it said it reads has
//! changed, or when anything in its package has if it said nothing, and then compiles the
//! package again whatever the script printed. The daemon runs the script the same way and
//! looks at what it printed and wrote: when that is what it was, the crates compiled with
//! it are still right, and the build stays the daemon's.
//!
//! To run a script as cargo does, the wrapper puts a few lines of shell where rustc linked
//! it. Cargo runs those, and they run `rustd build-script`, which writes down the
//! environment and the directory cargo ran it in and becomes the script.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The variable that names the script itself to `rustd build-script`.
const REAL: &str = "RUSTD_BUILD_SCRIPT";

/// The file next to a script's `OUT_DIR` that says how cargo ran it.
const RAN: &str = "rustd-ran.json";

/// How cargo ran a build script.
#[derive(Serialize, Deserialize)]
struct Ran {
    real: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
}

/// Has cargo run the script rustc linked at `linked` through `rustd build-script`.
#[cfg(unix)]
pub(crate) fn stand_in_front(linked: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let real = real_of(linked);
    std::fs::rename(linked, &real)?;
    let rustd = std::env::current_exe()?;
    let quoted = |path: &Path| -> anyhow::Result<String> {
        let path = path.to_str().context("a path that is not text")?;
        Ok(shlex::try_quote(path)?.into_owned())
    };
    let lines = format!(
        "#!/bin/sh\n{REAL}={} exec {} build-script \"$@\"\n",
        quoted(&real)?,
        quoted(&rustd)?
    );
    std::fs::write(linked, lines)?;
    Ok(std::fs::set_permissions(linked, std::fs::Permissions::from_mode(0o755))?)
}

/// Where there is no shell to run a script through, cargo runs the script itself, and a
/// change it reruns for is cargo's to build.
#[cfg(not(unix))]
pub(crate) fn stand_in_front(_: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn real_of(linked: &Path) -> PathBuf {
    let mut real = linked.as_os_str().to_owned();
    real.push(".real");
    PathBuf::from(real)
}

/// `rustd build-script`: writes down how cargo runs the script and becomes it.
pub(crate) fn command(args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let real = PathBuf::from(std::env::var_os(REAL).context("no build script is named")?);
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter(|(name, _)| name != REAL)
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let out_dir =
        env.iter().find(|(name, _)| name == "OUT_DIR").map(|(_, value)| PathBuf::from(value));
    if let Some(run_dir) = out_dir.as_deref().and_then(Path::parent) {
        let ran = Ran { real: real.clone(), cwd: std::env::current_dir()?, env };
        // Without it the script is not run again by the daemon, and cargo builds.
        let _ = std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran)?);
    }
    let mut script = Command::new(&real);
    script.args(args).env_remove(REAL);
    become_it(&mut script)
}

#[cfg(unix)]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    Err(std::os::unix::process::CommandExt::exec(script).into())
}

#[cfg(not(unix))]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    std::process::exit(script.status()?.code().unwrap_or(1))
}

/// Whether a script can be run again as cargo ran it into `out_dir`.
pub(crate) fn can_rerun(out_dir: &Path) -> bool {
    out_dir.parent().is_some_and(|it| it.join(RAN).exists())
}

/// Runs the script that wrote `out_dir` again, as cargo ran it. `Ok` when it printed and
/// wrote what it did then; `Err` is what is different.
pub(crate) fn rerun_is_same(out_dir: &Path) -> Result<(), String> {
    let run_dir = out_dir.parent().ok_or("a build script without a directory")?;
    let read = |name: &str| std::fs::read(run_dir.join(name)).map_err(|it| format!("{name}: {it}"));
    let ran: Ran = serde_json::from_slice(&read(RAN)?).map_err(|it| it.to_string())?;
    let printed = read("output")?;
    let wrote = digest_of(out_dir);
    let mut script = Command::new(&ran.real);
    script.env_clear().envs(ran.env).current_dir(&ran.cwd).stdin(Stdio::null());
    let again = script.output().map_err(|it| it.to_string())?;
    if !again.status.success() {
        return Err(format!("failed ({})", again.status));
    }
    if again.stdout != printed {
        return Err("printed something else".to_owned());
    }
    if digest_of(out_dir) != wrote {
        return Err("wrote something else".to_owned());
    }
    Ok(())
}

/// A hash of the names and the contents of the files below `dir`.
fn digest_of(dir: &Path) -> Vec<u8> {
    let mut hash = Sha256::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).into_iter().flatten().flatten();
        let mut paths: Vec<PathBuf> = entries.map(|it| it.path()).collect();
        paths.sort();
        for path in paths {
            hash.update(path.as_os_str().as_encoded_bytes());
            match path.is_dir() {
                true => pending.push(path),
                false => hash.update(std::fs::read(&path).unwrap_or_default()),
            }
        }
    }
    hash.finalize().to_vec()
}
