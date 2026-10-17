//! The records of what rustc built of the workspace. The wrapper writes one for every crate
//! cargo has it compile, next to cargo's own output for the profile, so `cargo clean` takes
//! both. The command reads them to know what a program is linked from.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// The directory of the records, in the directory cargo builds a profile in.
const RECORDS: &str = ".rustd-units";

/// The directories of a profile that cargo has rustc write to.
const PLACES: [&str; 3] = ["deps", "examples", "build"];

/// A stamp dir of a unit that no state names is deleted once it is this old: a build under
/// way may be about to name it.
const UNUSED_FOR: Duration = Duration::from_secs(3600);

/// One run of rustc on a crate of the workspace that succeeded.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Record {
    /// The crate's name and cargo's hash of the unit, as in the names of its files.
    pub(crate) id: String,
    pub(crate) crate_name: String,
    /// The directory rustc ran in, which the paths in its arguments are relative to.
    pub(crate) cwd: PathBuf,
    /// The arguments cargo passed, without the ones the wrapper added.
    pub(crate) args: Vec<String>,
    /// The root file of the crate.
    pub(crate) source: PathBuf,
    /// When rustc was done, in nanoseconds since the epoch. Later records have later stamps.
    pub(crate) stamp: u128,
    /// The files rustc wrote that a later build writes again, each with the copy of it.
    pub(crate) copies: Vec<Copied>,
    /// The copy of the line rustc ran the linker with, for a crate that is linked.
    pub(crate) link_line: Option<PathBuf>,
}

/// A file rustc wrote and the copy the daemon reads in its place.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Copied {
    pub(crate) original: PathBuf,
    pub(crate) copy: PathBuf,
}

impl Record {
    /// Where the records of the units cargo writes to `out_dir` are: in the directory of the
    /// profile, which `deps`, `examples` and `build` are in. A file cargo links from one of
    /// those to the profile's directory is found the same way.
    pub(crate) fn dir_for(out_dir: &Path) -> PathBuf {
        let profile = out_dir
            .ancestors()
            .find(|it| it.file_name().is_some_and(|name| PLACES.iter().any(|it| name == *it)))
            .and_then(Path::parent)
            .unwrap_or(out_dir);
        profile.join(RECORDS)
    }

    /// The record among `records` of the unit that wrote `artifact`, which may be cargo's
    /// hard link or copy of the file.
    pub(crate) fn of_artifact<'a>(
        records: &'a [Record],
        artifact: &Path,
        target_name: &str,
        src_path: &Path,
        is_test: bool,
        features: &[String],
    ) -> Option<&'a Record> {
        if let Some(found) = records.iter().find(|record| {
            record.outputs().iter().any(|it| same_file::is_same_file(it, artifact).unwrap_or(false))
        }) {
            return Some(found);
        }
        let crate_name = target_name.replace('-', "_");
        let src_canon = dunce::canonicalize(src_path).ok();
        let mut want_features = features.to_vec();
        want_features.sort();
        records
            .iter()
            .filter(|record| {
                record.is_linked()
                    && record.is_test() == is_test
                    && record.crate_name == crate_name
                    && (record.source == src_path
                        || dunce::canonicalize(&record.source).ok() == src_canon)
                    && record.features() == want_features
            })
            .max_by_key(|record| {
                let same_len = record.outputs().iter().any(|it| {
                    std::fs::metadata(it).ok().map(|m| m.len())
                        == std::fs::metadata(artifact).ok().map(|m| m.len())
                });
                (same_len, record.stamp)
            })
    }

    pub(crate) fn features(&self) -> Vec<String> {
        let mut features: Vec<String> = self
            .values("--cfg")
            .filter_map(|it| it.strip_prefix("feature=\"")?.strip_suffix('"'))
            .map(str::to_owned)
            .collect();
        features.sort();
        features
    }

    /// Every record in `dir`.
    pub(crate) fn all(dir: &Path) -> anyhow::Result<Vec<Record>> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Ok(Vec::new()) };
        let mut records = Vec::new();
        for entry in entries {
            let path = entry?.path().join("record.json");
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let record = serde_json::from_str(&text)
                .with_context(|| format!("{} is not a record", path.display()))?;
            records.push(record);
        }
        Ok(records)
    }

    /// Writes the record where the next reader finds it whole.
    pub(crate) fn write(&self, dir: &Path) -> anyhow::Result<()> {
        let unit = dir.join(&self.id);
        std::fs::create_dir_all(&unit)?;
        let new = unit.join(format!("record.{}.new", std::process::id()));
        std::fs::write(&new, serde_json::to_vec(self)?)?;
        std::fs::rename(&new, unit.join("record.json"))?;
        Ok(())
    }

    fn value(&self, flag: &str) -> Option<&str> {
        let at = self.args.iter().position(|it| it == flag)?;
        self.args.get(at + 1).map(String::as_str)
    }

    fn values<'a>(&'a self, flag: &'a str) -> impl Iterator<Item = &'a str> {
        self.args.windows(2).filter(move |it| it[0] == flag).map(|it| it[1].as_str())
    }

    /// The value of `-C name=value`, however cargo spelled it.
    pub(crate) fn codegen(&self, name: &str) -> Option<&str> {
        let joined = self.args.iter().filter_map(|it| it.strip_prefix("-C"));
        let split = self.values("-C");
        joined.chain(split).filter(|it| !it.is_empty()).find_map(|it| {
            let (key, value) = it.split_once('=').unwrap_or((it, ""));
            (key == name).then_some(value)
        })
    }

    pub(crate) fn out_dir(&self) -> Option<PathBuf> {
        self.value("--out-dir").map(|it| self.cwd.join(it))
    }

    fn extra_filename(&self) -> &str {
        self.codegen("extra-filename").unwrap_or_default()
    }

    pub(crate) fn is_test(&self) -> bool {
        self.args.iter().any(|it| it == "--test")
    }

    fn crate_types(&self) -> Vec<&str> {
        self.values("--crate-type").collect()
    }

    /// Whether rustc links an executable from the crate.
    pub(crate) fn is_linked(&self) -> bool {
        self.is_test() || self.crate_types() == ["bin"]
    }

    pub(crate) fn is_rlib(&self) -> bool {
        !self.is_test() && self.crate_types().iter().any(|it| matches!(*it, "lib" | "rlib"))
    }

    pub(crate) fn is_build_script(&self) -> bool {
        self.crate_name.starts_with("build_script_")
    }

    /// Whether the crate runs during the build: a build script or a proc macro.
    pub(crate) fn runs_in_build(&self) -> bool {
        self.is_build_script() || self.crate_types().contains(&"proc-macro")
    }

    /// The files of this unit that another unit or the user is given.
    pub(crate) fn outputs(&self) -> Vec<PathBuf> {
        let Some(out_dir) = self.out_dir() else { return Vec::new() };
        let (name, extra) = (&self.crate_name, self.extra_filename());
        let mut outputs = Vec::new();
        if self.is_linked() {
            outputs.push(out_dir.join(format!("{name}{extra}{}", std::env::consts::EXE_SUFFIX)));
        }
        if self.is_rlib() {
            outputs.push(out_dir.join(format!("lib{name}{extra}.rlib")));
        }
        if self.crate_types().contains(&"proc-macro") {
            let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
            outputs.push(out_dir.join(format!("{prefix}{name}{extra}{suffix}")));
        }
        outputs
    }

    /// The files of other units this one was compiled against: what `--extern` names.
    pub(crate) fn externs(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.values("--extern")
            .filter_map(|it| it.split_once('='))
            .map(|(_, path)| self.cwd.join(path))
    }

    /// The source files rustc read and the environment variables it asked for, from the
    /// dep-info cargo had it write.
    pub(crate) fn inputs(&self) -> Inputs {
        let path = self
            .out_dir()
            .map(|it| it.join(format!("{}{}.d", self.crate_name, self.extra_filename())));
        let text = path.and_then(|it| std::fs::read_to_string(it).ok()).unwrap_or_default();
        Inputs::parse(&text, &self.cwd)
    }

    /// The copy of `original`, when it is a file of this unit.
    pub(crate) fn copy_of(&self, original: &Path) -> Option<&Path> {
        self.copies.iter().find(|it| it.original == original).map(|it| &*it.copy)
    }
}

/// What a crate was compiled from besides its arguments.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct Inputs {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) env: Vec<(String, Option<String>)>,
}

impl Inputs {
    /// Reads a dep-info file: rules of make, `target: file file`, where a space in a name has
    /// a backslash before it, and comments `# env-dep:NAME=value` for the variables. There is
    /// no crate for the format: cargo has its own reader, which is not a library.
    fn parse(text: &str, cwd: &Path) -> Inputs {
        let mut inputs = Inputs::default();
        for line in text.lines() {
            if let Some(variable) = line.strip_prefix("# env-dep:") {
                let (name, value) = match variable.split_once('=') {
                    Some((name, value)) => (name, Some(value.to_owned())),
                    None => (variable, None),
                };
                inputs.env.push((name.to_owned(), value));
                continue;
            }
            let Some((_, files)) = line.split_once(": ") else { continue };
            if line.starts_with('#') {
                continue;
            }
            let mut file = String::new();
            for part in files.split(' ') {
                file.push_str(part);
                if let Some(escaped) = file.strip_suffix('\\') {
                    file = format!("{escaped} ");
                    continue;
                }
                if !file.is_empty() {
                    inputs.files.push(cwd.join(std::mem::take(&mut file)));
                }
            }
        }
        inputs.files.sort();
        inputs.files.dedup();
        inputs
    }
}

/// Now, as a stamp.
pub(crate) fn stamp_now() -> u128 {
    stamp_of(SystemTime::now())
}

pub(crate) fn stamp_of(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH).map_or(0, |it| it.as_nanos())
}

/// Deletes the copies in `dir` that no record names and none of `used` is in, once they are
/// old enough that no build under way is about to name them.
pub(crate) fn collect_garbage(dir: &Path, used: &[PathBuf]) -> anyhow::Result<()> {
    let now = stamp_now();
    for record in Record::all(dir)? {
        let unit = dir.join(&record.id);
        for entry in std::fs::read_dir(&unit)? {
            let path = entry?.path();
            let Some(stamp) = path.file_name().and_then(|it| it.to_str()?.parse::<u128>().ok())
            else {
                continue;
            };
            let named = stamp == record.stamp || used.iter().any(|it| it.starts_with(&path));
            if !named && now.saturating_sub(stamp) > UNUSED_FOR.as_nanos() {
                // Space, not correctness: a directory that stays is found again next time.
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dep_info_files_and_variables() {
        let text = "/t/deps/a-1.d: src/lib.rs src/with\\ space.rs /abs/gen.rs\n\n\
                    src/lib.rs:\n# env-dep:FOO=bar\n# env-dep:UNSET\n";
        let inputs = Inputs::parse(text, Path::new("/ws"));
        assert_eq!(
            inputs.files,
            ["/abs/gen.rs", "/ws/src/lib.rs", "/ws/src/with space.rs"].map(PathBuf::from)
        );
        assert_eq!(
            inputs.env,
            [("FOO".to_owned(), Some("bar".to_owned())), ("UNSET".to_owned(), None)]
        );
    }

    #[test]
    fn dep_info_that_is_empty_or_cut_short() {
        assert_eq!(Inputs::parse("", Path::new("/ws")), Inputs::default());
        assert_eq!(Inputs::parse("/t/a.d:", Path::new("/ws")), Inputs::default());
        assert_eq!(
            Inputs::parse("/t/a.d: src/cut\\", Path::new("/ws")).files,
            Vec::<PathBuf>::new()
        );
    }

    fn record(args: &[&str]) -> Record {
        Record {
            id: "a-1".to_owned(),
            crate_name: "a".to_owned(),
            cwd: PathBuf::from("/ws"),
            args: args.iter().map(|it| it.to_string()).collect(),
            source: PathBuf::from("/ws/src/lib.rs"),
            stamp: 1,
            copies: Vec::new(),
            link_line: None,
        }
    }

    #[test]
    fn records_are_in_the_profile() {
        let records = Path::new("/t/debug/.rustd-units");
        for dir in ["/t/debug/deps", "/t/debug/examples", "/t/debug/build/a-1", "/t/debug"] {
            assert_eq!(Record::dir_for(Path::new(dir)), records, "{dir}");
        }
    }

    #[test]
    fn codegen_flags_in_both_spellings() {
        let unit = record(&["-C", "opt-level=3", "-Cextra-filename=-1", "-C", "debug-assertions"]);
        assert_eq!(unit.codegen("opt-level"), Some("3"));
        assert_eq!(unit.codegen("extra-filename"), Some("-1"));
        assert_eq!(unit.codegen("debug-assertions"), Some(""));
        assert_eq!(unit.codegen("lto"), None);
    }

    #[test]
    fn outputs_by_kind() {
        let lib =
            record(&["--crate-type", "lib", "--out-dir", "t/deps", "-C", "extra-filename=-1"]);
        assert_eq!(lib.outputs(), [PathBuf::from("/ws/t/deps/liba-1.rlib")]);
        assert!(lib.is_rlib() && !lib.is_linked() && !lib.runs_in_build());
        let test = record(&["--crate-type", "lib", "--test", "--out-dir", "/t/deps"]);
        assert!(test.is_linked() && !test.is_rlib());
        let macros = record(&["--crate-type", "proc-macro"]);
        assert!(macros.runs_in_build() && !macros.is_linked());
    }
}
