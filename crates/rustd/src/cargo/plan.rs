//! What the daemon is asked to build a program from: the crates of the workspace that are in
//! it, the files rustc built to read symbols from, and rustc's link line with the daemon's
//! objects ahead of rustc's.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::units::Record;

/// What the linker is told so that the first definition of a name is the one that stays.
const FIRST_DEFINITION_STAYS: &str = "-Wl,--allow-multiple-definition";

/// The crates of the standard library whose symbols the daemon reads.
const LIBRARY: [&str; 3] = ["core", "alloc", "std"];

/// A program as the daemon builds it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Plan {
    /// The root files of the crates of the workspace that rustd compiles, the program's own
    /// first.
    pub(crate) crates: Vec<PathBuf>,
    /// The object with all of rustd's code for the program.
    pub(crate) object: PathBuf,
    /// The object with what an edit changed.
    pub(crate) patch: PathBuf,
    /// The executable the daemon links and splices.
    pub(crate) binary: PathBuf,
    /// The command that links `binary`, for a shell.
    pub(crate) link_cmd: String,
    pub(crate) symbol_files: Vec<PathBuf>,
    /// Whether the program's own crate is compiled with `cfg(test)` and is not a test target:
    /// the unit tests of a library or a binary.
    pub(crate) unit_test: bool,
    /// Source rustc compiled into the program that rustd does not: a change to one of these
    /// is cargo's to build.
    pub(crate) left_to_rustc: Vec<PathBuf>,
    /// The copies of rustc's output the plan names, which have to stay.
    pub(crate) copies: Vec<PathBuf>,
    /// The newest record the plan was made from.
    pub(crate) stamp: u128,
}

impl Plan {
    /// Plans the program `unit` built, with the daemon's files for it in `dir`. `records` are
    /// the units of the workspace. `Err` is why the program is left to cargo.
    pub(crate) fn of(
        unit: &Record,
        records: &[Record],
        dir: &Path,
        unit_test: bool,
    ) -> anyhow::Result<Result<Plan, String>> {
        if let Some(why) = unsupported(unit) {
            return Ok(Err(why));
        }
        let line = unit.link_line.as_ref().context("a linked unit without a link line")?;
        let line =
            std::fs::read_to_string(line).with_context(|| format!("reading {}", line.display()))?;
        let Some(mut link) = LinkLine::parse(&line) else {
            return Ok(Err("the link line is not one of an ELF linker driven by `cc`".to_owned()));
        };
        let mut plan = Plan {
            crates: vec![unit.source.clone()],
            object: dir.join("rustd.o"),
            patch: dir.join("patch.o"),
            binary: dir.join("bin"),
            link_cmd: String::new(),
            symbol_files: Vec::new(),
            unit_test,
            left_to_rustc: Vec::new(),
            copies: Vec::new(),
            stamp: unit.stamp,
        };
        for arg in &mut link.args {
            let path = PathBuf::from(&*arg);
            let member = records.iter().find(|it| it.is_rlib() && it.outputs().contains(&path));
            let Some(owner) = member.or_else(|| unit.copy_of(&path).map(|_| unit)) else {
                if link.macho
                    && is_rustc_file(arg)
                    && !path.components().any(|it| it.as_os_str() == "rustlib")
                    && path.exists()
                    && let Ok(weak) = crate::weaken::weaken_file(&path)
                {
                    *arg = weak.display().to_string();
                }
                continue;
            };
            if let Some(why) = unsupported(owner) {
                return Ok(Err(why));
            }
            let Some(copy) = owner.copy_of(&path) else {
                return Ok(Err(format!("no copy of {} was kept", path.display())));
            };
            plan.copies.push(copy.to_owned());
            plan.stamp = plan.stamp.max(owner.stamp);
            *arg = copy.display().to_string();
            if member.is_none() {
                continue;
            }
            // With `cfg(test)` the daemon has it on every crate of the workspace, and rustc
            // built the libraries of a unit test without it.
            if unit_test && std::env::var_os("RUSTD_FULL").is_some() {
                plan.left_to_rustc.extend(owner.inputs().files);
            } else {
                plan.crates.push(owner.source.clone());
            }
        }
        plan.symbol_files = link.rustc_files();
        std::fs::create_dir_all(dir)?;
        plan.link_cmd = link.command(&plan, &dir.join("link.args"))?;
        Ok(Ok(plan))
    }
}

/// Why rustd has no part in a crate that was compiled with these flags, when it has none.
fn unsupported(unit: &Record) -> Option<String> {
    let name = &unit.crate_name;
    let off = |value: Option<&str>| matches!(value, None | Some("off" | "no" | "n" | "false"));
    if std::env::var_os("RUSTD_STRICT_OPT_LEVEL").is_some()
        && !matches!(unit.codegen("opt-level"), None | Some("0"))
    {
        let level = unit.codegen("opt-level").unwrap_or_default();
        return Some(format!("`{name}` is built with opt-level={level}"));
    }
    if !off(unit.codegen("lto")) || unit.codegen("linker-plugin-lto").is_some() {
        return Some(format!("`{name}` is built with LTO"));
    }
    if unit.codegen("panic") == Some("abort") {
        return Some(format!("`{name}` is built with panic=abort"));
    }
    for flag in
        ["instrument-coverage", "target-cpu", "target-feature", "profile-generate", "profile-use"]
    {
        if unit.codegen(flag).is_some() {
            return Some(format!("`{name}` is built with -C {flag}"));
        }
    }
    let allowed_z = |z: &str| {
        let key = z.split('=').next().unwrap_or(z).trim();
        matches!(
            key,
            "threads"
                | "share-generics"
                | "embed-metadata"
                | "macro-backtrace"
                | "proc-macro-backtrace"
                | "time-passes"
                | "self-profile"
                | "unstable-options"
        )
    };
    let mut i = 0;
    while i < unit.args.len() {
        let arg = &unit.args[i];
        if arg == "-Z" {
            let val = unit.args.get(i + 1).map_or("", String::as_str);
            if !allowed_z(val) {
                return Some(format!("`{name}` is built with unstable flag -Z {val}"));
            }
            i += 2;
            continue;
        } else if let Some(rest) = arg.strip_prefix("-Z") {
            if !allowed_z(rest) {
                return Some(format!("`{name}` is built with unstable flag {arg}"));
            }
        }
        i += 1;
    }
    None
}

/// The line rustc ran the linker with: the variables it set, the driver, the arguments.
struct LinkLine {
    env: Vec<(String, String)>,
    linker: String,
    args: Vec<String>,
    macho: bool,
}

impl LinkLine {
    /// `None` for a line that is not `cc`'s for an ELF or Mach-O linker.
    fn parse(line: &str) -> Option<LinkLine> {
        let words = shlex::split(line.lines().find(|it| it.contains('"'))?)?;
        let mut first = 0;
        let mut env = Vec::new();
        while let Some(word) = words.get(first) {
            if word == "env" {
                first += 1;
            } else if word == "-u" {
                first += 2;
            } else if is_assignment(word)
                && let Some((name, value)) = word.split_once('=')
            {
                env.push((name.to_owned(), value.to_owned()));
                first += 1;
            } else {
                break;
            }
        }
        let linker = words.get(first)?.clone();
        let args: Vec<String> = words[first + 1..]
            .iter()
            // It exports what a dynamic library would, and is gone with rustc's directory.
            .filter(|it| !it.ends_with("symbols.o"))
            .cloned()
            .collect();
        let macho = args.iter().any(|it| it == "-arch");
        let supported = args.iter().any(|it| it == "-o")
            && !args.iter().any(|it| it == "/NOLOGO" || it == "-flavor");
        supported.then(|| LinkLine { env, linker, args, macho })
    }

    /// What rustc built that is linked, and the standard library's crates next to it.
    fn rustc_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> =
            self.args.iter().filter(|it| is_rustc_file(it)).map(PathBuf::from).collect();
        let library = files.iter().find_map(|it| {
            it.components().any(|it| it.as_os_str() == "rustlib").then(|| it.parent())?
        });
        let entries = library.and_then(|it| std::fs::read_dir(it).ok()).into_iter().flatten();
        for path in entries.flatten().map(|it| it.path()) {
            let name = path.file_name().and_then(|it| it.to_str()).unwrap_or_default();
            let of_library = LIBRARY.iter().any(|it| {
                name.strip_prefix("lib")
                    .and_then(|rest| rest.strip_prefix(it))
                    .is_some_and(|rest| rest.starts_with('-'))
            });
            if of_library && name.ends_with(".rlib") && !files.contains(&path) {
                files.push(path);
            }
        }
        files
    }

    /// The command that links the plan's executable, with its arguments in the file at
    /// `response`: a line with all of them is longer than a process may be given.
    fn command(mut self, plan: &Plan, response: &Path) -> anyhow::Result<String> {
        let first = self
            .args
            .iter()
            .position(|it| is_rustc_file(it))
            .context("nothing of rustc's is linked")?;
        let ours = [&plan.patch, &plan.object].map(|it| it.display().to_string());
        let flag = if self.macho { "-Wl,-headerpad,0x200" } else { FIRST_DEFINITION_STAYS };
        self.args.splice(first..first, ours.into_iter().chain([flag.to_owned()]));
        let output =
            self.args.iter().position(|it| it == "-o").context("the link names no output")?;
        *self.args.get_mut(output + 1).context("the link names no output")? =
            plan.binary.display().to_string();
        let quoted: Vec<String> = self
            .args
            .iter()
            .map(|it| shlex::try_quote(it).map(|it| it.into_owned()))
            .collect::<Result<_, _>>()
            .context("an argument of the link can't be quoted")?;
        std::fs::write(response, quoted.join("\n"))?;
        let mut command = Vec::new();
        for (name, value) in &self.env {
            command.push(format!("{name}={}", shlex::try_quote(value)?));
        }
        command.push(shlex::try_quote(&self.linker)?.into_owned());
        command.push(shlex::try_quote(&format!("@{}", response.display()))?.into_owned());
        Ok(command.join(" "))
    }
}

/// Whether a word of the link line sets a variable for the linker's driver.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'_')
    })
}

fn is_rustc_file(arg: &str) -> bool {
    arg.ends_with(".rlib") || arg.ends_with(".rcgu.o")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"LC_ALL="C" PATH="/t/bin:/usr/bin" "cc" "-m64" "/t/deps/rustc1/symbols.o" "/t/deps/a.1.rcgu.o" "-Wl,-Bstatic" "/t/deps/libb-2.rlib" "-o" "/t/deps/a-1" "-pie""#;

    #[test]
    fn link_line_of_cc() {
        let link = LinkLine::parse(LINE).unwrap();
        assert_eq!(
            link.env,
            [
                ("LC_ALL".to_owned(), "C".to_owned()),
                ("PATH".to_owned(), "/t/bin:/usr/bin".to_owned())
            ]
        );
        assert_eq!(link.linker, "cc");
        assert_eq!(link.args[..2], ["-m64", "/t/deps/a.1.rcgu.o"]);
        assert_eq!(
            link.rustc_files(),
            ["/t/deps/a.1.rcgu.o", "/t/deps/libb-2.rlib"].map(PathBuf::from)
        );
    }

    #[test]
    fn link_lines_that_are_not_cc_for_elf() {
        assert!(LinkLine::parse("").is_none());
        assert!(LinkLine::parse("no quotes here").is_none());
        assert!(LinkLine::parse(r#""link.exe" "/NOLOGO" "/OUT:a.exe" "-o""#).is_none());
        assert!(LinkLine::parse(r#""rust-lld" "-flavor" "wasm" "-o" "a.wasm""#).is_none());
        assert!(LinkLine::parse(r#""cc" "-arch" "arm64" "-o" "a""#).is_some_and(|l| l.macho));
        assert!(LinkLine::parse(r#"A="1" "cc" "unterminated"#).is_none());
    }

    #[test]
    fn ours_go_ahead_of_rustcs() {
        let dir = std::env::temp_dir().join(format!("rustd-plan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plan = Plan {
            crates: Vec::new(),
            object: PathBuf::from("/p/rustd.o"),
            patch: PathBuf::from("/p/patch.o"),
            binary: PathBuf::from("/p/bin"),
            link_cmd: String::new(),
            symbol_files: Vec::new(),
            unit_test: false,
            left_to_rustc: Vec::new(),
            copies: Vec::new(),
            stamp: 0,
        };
        let response = dir.join("link.args");
        let command = LinkLine::parse(LINE).unwrap().command(&plan, &response).unwrap();
        assert_eq!(command, format!("LC_ALL=C PATH=/t/bin:/usr/bin cc @{}", response.display()));
        // As the driver reads the file: quoted where a shell would need it.
        let args = shlex::split(&std::fs::read_to_string(&response).unwrap()).unwrap();
        assert_eq!(
            args,
            [
                "-m64",
                "/p/patch.o",
                "/p/rustd.o",
                FIRST_DEFINITION_STAYS,
                "/t/deps/a.1.rcgu.o",
                "-Wl,-Bstatic",
                "/t/deps/libb-2.rlib",
                "-o",
                "/p/bin",
                "-pie"
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
