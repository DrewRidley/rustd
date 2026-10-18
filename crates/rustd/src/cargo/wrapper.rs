//! rustd as cargo's `RUSTC_WORKSPACE_WRAPPER` under `cargo rustd`: rustc runs as cargo asked,
//! and what it built of a crate of the workspace is recorded for the daemon. A crate that is
//! linked is also asked for its link line and for the objects it was linked from.

use std::{
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    script,
    units::{Copied, Record, stamp_now, stamp_of},
};
use crate::stub::VALUED;

/// The variable that makes `rustd` this wrapper.
pub(crate) const ENABLED: &str = "RUSTD_CARGO_WRAPS";
/// The variable that makes `rustd` the incremental Clippy workspace wrapper.
pub(crate) const CLIPPY_ENABLED: &str = "RUSTD_CLIPPY_WRAPS";
pub(crate) const CLIPPY_DRIVER: &str = "RUSTD_CLIPPY_DRIVER";
pub(crate) const CLIPPY_EXTRA_ARGS: &str = "RUSTD_CLIPPY_EXTRA_ARGS";

#[derive(Serialize, Deserialize)]
struct ClippyCache {
    full_hash: String,
    outline_hash: String,
    files: Vec<PathBuf>,
    env: Vec<(String, Option<String>)>,
    outputs: Vec<(PathBuf, PathBuf)>,
    stderr: Vec<u8>,
}

/// Runs rustc (or clippy-driver), records the unit when it succeeded, and ends as it did.
pub(crate) fn command(mut args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    if std::env::var_os(CLIPPY_ENABLED).is_some() {
        return clippy_command(args);
    }
    let rustc = args.next().context("cargo names rustc first")?;
    let args: Vec<OsString> = args.collect();
    let Some(unit) = Unit::of(&args) else {
        let status = Command::new(&rustc).args(&args).status()?;
        std::process::exit(status.code().unwrap_or(1));
    };
    let stamp = stamp_now();
    let copies = unit.records.join(&unit.id).join(stamp.to_string());
    let line = copies.join("link.line");
    let mut command = Command::new(&rustc);
    command.args(&args);
    let initial_record = unit.as_record(0, Vec::new(), None);
    if !initial_record.runs_in_build()
        && !matches!(initial_record.codegen("opt-level"), None | Some("0"))
    {
        command.arg("-Copt-level=0");
        if initial_record.codegen("debug-assertions").is_none() {
            command.arg("-Cdebug-assertions=yes");
        }
    }
    if unit.linked {
        std::fs::create_dir_all(&copies)?;
        command.arg(format!("--print=link-args={}", line.display())).arg("-Csave-temps");
        if cfg!(target_os = "macos") {
            command.arg("-Clink-arg=-Wl,-headerpad,0x200");
        }
    }
    let status =
        command.status().with_context(|| format!("running {}", Path::new(&rustc).display()))?;
    if status.success()
        && let Err(error) = unit.record(stamp, &copies, unit.linked.then_some(line))
    {
        // The build is good; the daemon has no part in this crate until it is built again.
        eprintln!("warning: rustd did not record `{}`: {error:#}", unit.id);
    }
    std::process::exit(status.code().unwrap_or(1));
}

fn clippy_command(mut args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let rustc = args.next().context("cargo names rustc first")?;
    let args: Vec<OsString> = args.collect();
    let driver =
        std::env::var_os(CLIPPY_DRIVER).unwrap_or_else(|| OsString::from("clippy-driver"));
    let Some(unit) = Unit::of(&args) else {
        let status = Command::new(&rustc).args(&args).status()?;
        std::process::exit(status.code().unwrap_or(1));
    };
    if unit.as_record(0, Vec::new(), None).runs_in_build() {
        let status = Command::new(&rustc).args(&args).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    if unit.try_replay_clippy()? {
        std::process::exit(0);
    }
    let extra: Vec<String> = std::env::var(CLIPPY_EXTRA_ARGS)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let unit_dir = unit.records.join(&unit.id);
    let known_files = unit.known_clippy_files();
    if let Some(shadow) =
        super::clippy::prepare_shadow(&unit.cwd, &unit.source, &unit_dir, &known_files)
    {
        let mut shadow_args = unit.args.clone();
        if let Some(src_idx) = (0..shadow_args.len()).find(|&at| {
            shadow_args[at].ends_with(".rs")
                && !(at > 0 && VALUED.contains(&shadow_args[at - 1].as_str()))
        }) {
            shadow_args[src_idx] = shadow.shadow_source.display().to_string();
        }
        shadow_args.push(format!("--remap-path-prefix={}/=", shadow.shadow_root.display()));
        let output = Command::new(&driver)
            .arg(&rustc)
            .args(&shadow_args)
            .args(&extra)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::piped())
            .output()
            .with_context(|| format!("running {}", Path::new(&driver).display()))?;
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        if output.status.success() || !stderr_str.contains("{ loop {}") {
            unit.rewrite_shadow_dep_info(&shadow.shadow_root);
            if std::env::var_os("RUSTD_CLIPPY_VERBOSE").is_some() {
                eprintln!(
                    "[rustd clippy] {}: linted {} changed fn(s), elided {} unchanged fn(s)",
                    unit.crate_name, shadow.kept_changed_fns, shadow.elided_fns
                );
            }
            std::io::stderr().write_all(&output.stderr)?;
            if output.status.success() {
                let _ = unit.record_clippy(&output.stderr);
            }
            std::process::exit(output.status.code().unwrap_or(1));
        }
    }

    let output = Command::new(&driver)
        .arg(&rustc)
        .args(&args)
        .args(&extra)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("running {}", Path::new(&driver).display()))?;
    std::io::stderr().write_all(&output.stderr)?;
    if output.status.success() {
        let _ = unit.record_clippy(&output.stderr);
    }
    std::process::exit(output.status.code().unwrap_or(1));
}

/// A run of rustc that compiles a crate, as cargo's arguments describe it.
struct Unit {
    id: String,
    crate_name: String,
    args: Vec<String>,
    source: PathBuf,
    cwd: PathBuf,
    records: PathBuf,
    /// Whether rustc links a program of the workspace from it.
    linked: bool,
    /// Whether rustc links a build script from it.
    script: bool,
}

impl Unit {
    /// `None` for a run that compiles nothing to record: a question to rustc (`--print`,
    /// `-vV`), source on standard input, or arguments that are not text.
    fn of(args: &[OsString]) -> Option<Unit> {
        let args: Vec<String> =
            args.iter().map(|it| it.to_str().map(str::to_owned)).collect::<Option<_>>()?;
        let value = |flag: &str| {
            let at = args.iter().position(|it| it == flag)?;
            args.get(at + 1).cloned()
        };
        let crate_name = value("--crate-name")?;
        let out_dir = value("--out-dir")?;
        if args.iter().any(|it| it == "-" || it.starts_with("--print")) {
            return None;
        }
        // The one argument that is a path to a Rust file and not the value of a flag.
        let source = (0..args.len()).find(|&at| {
            args[at].ends_with(".rs") && !(at > 0 && VALUED.contains(&args[at - 1].as_str()))
        })?;
        let cwd = std::env::current_dir().ok()?;
        let mut unit = Unit {
            id: String::new(),
            crate_name,
            source: cwd.join(&args[source]),
            records: Record::dir_for(&cwd.join(out_dir)),
            args,
            cwd,
            linked: false,
            script: false,
        };
        let record = unit.as_record(0, Vec::new(), None);
        let emits_link = unit.args.iter().any(|it| {
            it.strip_prefix("--emit=").is_some_and(|kinds| kinds.split(',').any(|it| it == "link"))
        });
        unit.script = record.is_build_script() && emits_link;
        unit.linked = record.is_linked() && emits_link && !record.runs_in_build();
        unit.id =
            format!("{}{}", unit.crate_name, record.codegen("extra-filename").unwrap_or_default());
        Some(unit)
    }

    fn as_record(&self, stamp: u128, copies: Vec<Copied>, link_line: Option<PathBuf>) -> Record {
        Record {
            id: self.id.clone(),
            crate_name: self.crate_name.clone(),
            cwd: self.cwd.clone(),
            args: self.args.clone(),
            source: self.source.clone(),
            stamp,
            copies,
            link_line,
        }
    }

    /// Copies what a later build of the unit writes over and writes the record.
    fn record(&self, stamp: u128, to: &Path, link_line: Option<PathBuf>) -> anyhow::Result<()> {
        let record = self.as_record(stamp, Vec::new(), link_line.clone());
        // The executable is the daemon's to link; of the outputs it reads the libraries.
        let libraries =
            record.outputs().into_iter().filter(|it| it.extension().is_some_and(|it| it == "rlib"));
        let mut originals: Vec<PathBuf> = libraries.filter(|it| it.exists()).collect();
        if let Some(line) = &link_line {
            let line = std::fs::read_to_string(line)
                .with_context(|| format!("rustc wrote no link line to {}", line.display()))?;
            let args = shlex::split(&line).context("the link line is not quoted as a shell's")?;
            let objects = args.into_iter().filter(|it| it.ends_with(".rcgu.o")).map(PathBuf::from);
            originals.extend(objects.filter(|it| it.exists()));
        }
        std::fs::create_dir_all(to)?;
        let mut copies = Vec::new();
        for original in originals {
            let name = original.file_name().context("an output without a name")?;
            let copy = to.join(name);
            if cfg!(target_os = "macos") {
                let mut data = std::fs::read(&original)
                    .with_context(|| format!("reading {}", original.display()))?;
                crate::weaken::weaken(&mut data);
                std::fs::write(&copy, &data)
                    .with_context(|| format!("writing {}", copy.display()))?;
            } else {
                std::fs::copy(&original, &copy)
                    .with_context(|| format!("copying {}", original.display()))?;
            }
            copies.push(Copied { original, copy });
        }
        // The daemon tells which crate a file of rustc's is by the dep-info next to it, of
        // which it reads the first source: the root of the crate.
        let stem =
            format!("{}{}", self.crate_name, record.codegen("extra-filename").unwrap_or_default());
        std::fs::write(
            to.join(format!("{stem}.d")),
            format!("{stem}: {}\n", self.source.display()),
        )?;
        if self.script
            && let Some(linked) = record.outputs().first()
        {
            script::stand_in_front(linked)?;
        }
        self.as_record(stamp, copies, link_line).write(&self.records)
    }

    fn clippy_hashes(
        &self,
        record: &Record,
        files: &[PathBuf],
        env: &[(String, Option<String>)],
    ) -> Option<(String, String)> {
        let mut full = Sha256::new();
        let mut outline = Sha256::new();
        for arg in &self.args {
            full.update(arg.as_bytes());
            full.update([0]);
            outline.update(arg.as_bytes());
            outline.update([0]);
        }
        if let Ok(extra) = std::env::var(CLIPPY_EXTRA_ARGS) {
            full.update(extra.as_bytes());
            full.update([0]);
            outline.update(extra.as_bytes());
            outline.update([0]);
        }
        for file in files {
            let bytes = std::fs::read(file).ok()?;
            full.update(file.as_os_str().as_encoded_bytes());
            full.update([0]);
            full.update(&bytes);
            full.update([0xff]);
            outline.update(file.as_os_str().as_encoded_bytes());
            outline.update([0]);
            if file.extension().is_some_and(|e| e == "rs")
                && let Ok(text) = std::str::from_utf8(&bytes)
            {
                let h = crate::outline::syntax_outline_hash(text);
                outline.update(h.to_le_bytes());
            } else {
                outline.update(&bytes);
            }
            outline.update([0xff]);
        }
        for (name, _) in env {
            let val = std::env::var(name).ok();
            full.update(name.as_bytes());
            full.update([0]);
            if let Some(v) = &val {
                full.update(v.as_bytes());
            }
            full.update([1]);
            outline.update(name.as_bytes());
            outline.update([0]);
            if let Some(v) = &val {
                outline.update(v.as_bytes());
            }
            outline.update([1]);
        }
        let outlines_dir = self.records.join("clippy-outlines");
        for ext in record.externs() {
            let ext_name = ext.file_name()?;
            let outline_file = outlines_dir.join(ext_name);
            full.update(ext_name.as_encoded_bytes());
            outline.update(ext_name.as_encoded_bytes());
            if let Ok(ext_outline) = std::fs::read(&outline_file) {
                full.update(&ext_outline);
                outline.update(&ext_outline);
            } else {
                let meta = std::fs::metadata(&ext).ok()?;
                let len = meta.len();
                let mtime = meta.modified().map(stamp_of).unwrap_or(0);
                full.update(len.to_le_bytes());
                full.update(mtime.to_le_bytes());
                outline.update(len.to_le_bytes());
                outline.update(mtime.to_le_bytes());
            }
        }
        let hex = |d: Sha256| -> String {
            d.finalize().iter().map(|b| format!("{b:02x}")).collect()
        };
        Some((hex(full), hex(outline)))
    }

    fn try_replay_clippy(&self) -> anyhow::Result<bool> {
        let unit_dir = self.records.join(&self.id);
        let cache_path = unit_dir.join("clippy-cache.json");
        let Ok(text) = std::fs::read_to_string(&cache_path) else {
            return Ok(false);
        };
        let Ok(cached) = serde_json::from_str::<ClippyCache>(&text) else {
            return Ok(false);
        };
        let record = self.as_record(0, Vec::new(), None);
        let Some((now_full, now_outline)) =
            self.clippy_hashes(&record, &cached.files, &cached.env)
        else {
            return Ok(false);
        };
        if now_full != cached.full_hash || cached.outputs.is_empty() {
            return Ok(false);
        }
        if !cached.outputs.iter().all(|(_, copy)| copy.exists()) {
            return Ok(false);
        }
        for (original, copy) in &cached.outputs {
            let bytes = std::fs::read(copy)?;
            crate::replace(original, &bytes)?;
        }
        let outlines_dir = self.records.join("clippy-outlines");
        let _ = std::fs::create_dir_all(&outlines_dir);
        for (original, _) in &cached.outputs {
            if let Some(name) = original.file_name() {
                let _ = std::fs::write(outlines_dir.join(name), &now_outline);
            }
        }
        std::io::stderr().write_all(&cached.stderr)?;
        Ok(true)
    }

    fn record_clippy(&self, stderr: &[u8]) -> anyhow::Result<()> {
        let record = self.as_record(0, Vec::new(), None);
        let Some(out_dir) = record.out_dir() else { return Ok(()) };
        let inputs = record.inputs();
        if inputs.files.is_empty() {
            return Ok(());
        }
        let Some((full_hash, outline_hash)) =
            self.clippy_hashes(&record, &inputs.files, &inputs.env)
        else {
            return Ok(());
        };
        let stem =
            format!("{}{}", self.crate_name, record.codegen("extra-filename").unwrap_or_default());
        let candidates = [
            out_dir.join(format!("{stem}.d")),
            out_dir.join(format!("lib{stem}.rmeta")),
            out_dir.join(format!("{stem}.rmeta")),
        ];
        let unit_dir = self.records.join(&self.id);
        if stderr.is_empty() {
            super::clippy::save_snapshot(&self.cwd, &unit_dir, &inputs.files);
        }
        let artifacts_dir = unit_dir.join("clippy-artifacts");
        std::fs::create_dir_all(&artifacts_dir)?;
        let outlines_dir = self.records.join("clippy-outlines");
        std::fs::create_dir_all(&outlines_dir)?;
        let mut outputs = Vec::new();
        for original in candidates {
            if original.exists()
                && let Some(name) = original.file_name()
            {
                let copy = artifacts_dir.join(name);
                std::fs::copy(&original, &copy)?;
                let _ = std::fs::write(outlines_dir.join(name), &outline_hash);
                outputs.push((original, copy));
            }
        }
        let cache = ClippyCache {
            full_hash,
            outline_hash,
            files: inputs.files,
            env: inputs.env,
            outputs,
            stderr: stderr.to_vec(),
        };
        let cache_path = unit_dir.join("clippy-cache.json");
        let new = unit_dir.join(format!("clippy-cache.{}.new", std::process::id()));
        std::fs::write(&new, serde_json::to_vec(&cache)?)?;
        std::fs::rename(&new, &cache_path)?;
        Ok(())
    }

    fn known_clippy_files(&self) -> Vec<PathBuf> {
        let record = self.as_record(0, Vec::new(), None);
        let inputs = record.inputs();
        if !inputs.files.is_empty() {
            return inputs.files;
        }
        let cache_path = self.records.join(&self.id).join("clippy-cache.json");
        if let Ok(text) = std::fs::read_to_string(&cache_path)
            && let Ok(cached) = serde_json::from_str::<ClippyCache>(&text)
        {
            return cached.files;
        }
        Vec::new()
    }

    fn rewrite_shadow_dep_info(&self, shadow_root: &Path) {
        let record = self.as_record(0, Vec::new(), None);
        let Some(out_dir) = record.out_dir() else { return };
        let stem =
            format!("{}{}", self.crate_name, record.codegen("extra-filename").unwrap_or_default());
        let dep_path = out_dir.join(format!("{stem}.d"));
        let Ok(text) = std::fs::read_to_string(&dep_path) else { return };
        let prefix = format!("{}/", shadow_root.display());
        let prefix_escaped = prefix.replace(' ', "\\ ");
        let replaced = text.replace(&prefix, "").replace(&prefix_escaped, "");
        if replaced != text {
            let _ = std::fs::write(&dep_path, replaced);
        }
    }
}
