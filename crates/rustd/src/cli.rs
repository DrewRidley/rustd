//! Cargo-style CLI (`rustd build`, `rustd test`, `rustd check`, `rustd clippy`, `rustd fmt`,
//! `rustd run`, `rustd start`, `rustd status`, `rustd stop`) built on top of `rustd`'s
//! multi-program, multi-worktree daemon (`Request::Sync` / `Request::Patch`, `splice.rs`,
//! `weaken.rs`, and `store.rs`).

use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, bail, ensure};

pub(crate) fn is_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "build"
            | "check"
            | "clippy"
            | "fmt"
            | "rebase"
            | "run"
            | "test"
            | "start"
            | "warm"
            | "status"
            | "stop"
            | "help"
            | "--help"
            | "-h"
    )
}

pub(crate) fn dispatch(sub: &str, args: Vec<String>) -> anyhow::Result<()> {
    match sub {
        "build" | "rebase" => cmd_build(&args),
        "check" => cmd_check(&args),
        "clippy" => cmd_clippy(&args),
        "fmt" => cmd_fmt(&args),
        "run" => cmd_run(&args),
        "test" => cmd_test(&args),
        "start" | "warm" => cmd_start(&args),
        "status" => cmd_status(&args),
        "stop" => cmd_stop(&args),
        _ => {
            eprintln!(
                "Usage:\n  \
                 rustd build  [-p PACKAGE] [--bin NAME] [--features FEATURES] [--watch]\n  \
                 rustd test   [FILTER] [-p PACKAGE] [--since SHA] [--junit PATH] [--json] [--also-run FILE] [--retries N] [--explain TEST] [--list-changed] [--backtest] [--faults N] [--watch] [-- ARGS...]\n  \
                 rustd check  [-p PACKAGE] [--bin NAME] [--test NAME] [--watch]\n  \
                 rustd clippy [-p PACKAGE] [--tests] [--watch] [-- ARGS...]\n  \
                 rustd fmt    [--check] [--since SHA]\n  \
                 rustd run    [-p PACKAGE] [--bin NAME] [-- ARGS...]\n  \
                 rustd start  [-p PACKAGE] [--bin NAME]\n  \
                 rustd status [--manifest-path PATH]\n  \
                 rustd stop   [--manifest-path PATH]\n\n\
                 Low-level / harness subcommands:\n  \
                 rustd sync <SOCKET> [--project <PATH> --crate <CRATE> --out <OBJ> --patch <PATCH> --link-cmd <CMD> --binary <BIN> --symbols <RLIB>...]\n  \
                 rustd patch <SOCKET> [...]\n  \
                 rustd weaken <FILE>...\n  \
                 rustd cache push|pull <REMOTE> [PROJECT]\n  \
                 rustd deps <PROJECT>"
            );
            Ok(())
        }
    }
}

#[derive(Clone)]
struct CliOpts {
    manifest_path: PathBuf,
    workspace_root: PathBuf,
    bin_name: String,
    pkg_name: Option<String>,
    test_target: Option<String>,
    crates: Vec<String>,
    exclude: Vec<String>,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    target: Option<String>,
    watch: bool,
    all_tests: bool,
    list_changed: bool,
    since: Option<String>,
    junit: Option<PathBuf>,
    json: bool,
    also_run: Option<PathBuf>,
    retries: Option<usize>,
    explain: Option<String>,
    backtest: bool,
    faults: usize,
    cfg_test: bool,
    full: bool,
    run_args: Vec<String>,
}

struct TargetPaths {
    dir: PathBuf,
    base_o: PathBuf,
    patch_o: PathBuf,
    out_bin: PathBuf,
    rustd_sock: PathBuf,
    rustd_log: PathBuf,
    pid_file: PathBuf,
    store_dir: PathBuf,
    link_args_txt: PathBuf,
    link_sh: PathBuf,
    rlibs_txt: PathBuf,
}

impl TargetPaths {
    fn new(workspace_root: &Path, bin_name: &str) -> Self {
        let rustd_root = workspace_root.join("target").join("rustd");
        let dir = rustd_root.join(bin_name);
        let rustd_sock = env::var_os("RUSTD_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| rustd_root.join("rustd.sock"));
        let store_dir = env::var_os("RUSTD_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|| rustd_root.join("store"));
        Self {
            base_o: dir.join("base.o"),
            patch_o: dir.join("patch.o"),
            out_bin: dir.join(bin_name),
            rustd_sock,
            rustd_log: rustd_root.join("rustd.log"),
            pid_file: rustd_root.join("rustd.pid"),
            store_dir,
            link_args_txt: dir.join("link_args.txt"),
            link_sh: dir.join("link.sh"),
            rlibs_txt: dir.join("rlibs.txt"),
            dir,
        }
    }
}

fn find_manifest_path(explicit: Option<PathBuf>) -> anyhow::Result<(PathBuf, PathBuf)> {
    if let Some(p) = explicit {
        let abs = dunce::canonicalize(&p).with_context(|| format!("finding {}", p.display()))?;
        let manifest = if abs.is_dir() { abs.join("Cargo.toml") } else { abs };
        let root = manifest.parent().context("invalid manifest path")?.to_path_buf();
        return Ok((manifest, root));
    }
    let mut cur = env::current_dir()?;
    let mut first_manifest = None;
    loop {
        let candidate = cur.join("Cargo.toml");
        if candidate.exists() {
            if first_manifest.is_none() {
                first_manifest = Some(candidate.clone());
            }
            if let Ok(txt) = fs::read_to_string(&candidate)
                && txt.contains("[workspace]")
            {
                return Ok((candidate, cur));
            }
        }
        if !cur.pop() {
            break;
        }
    }
    if let Some(m) = first_manifest {
        let root = m.parent().unwrap().to_path_buf();
        return Ok((m, root));
    }
    bail!("could not find Cargo.toml in current directory or any parent")
}

fn infer_default_bin(workspace_root: &Path) -> Option<String> {
    let rustd_dir = workspace_root.join("target").join("rustd");
    if let Ok(entries) = fs::read_dir(&rustd_dir) {
        let mut all_dirs = Vec::new();
        for e in entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
            if let Ok(name) = e.file_name().into_string()
                && name != "store"
            {
                all_dirs.push(name);
            }
        }
        if all_dirs.len() == 1 {
            return all_dirs.pop();
        }
    }
    let manifest = workspace_root.join("Cargo.toml");
    if let Ok(txt) = fs::read_to_string(manifest) {
        let mut in_bin = false;
        let mut pkg_name = None;
        for line in txt.lines() {
            let t = line.trim();
            if t.starts_with('[') {
                in_bin = t == "[[bin]]";
                continue;
            }
            if let Some(rest) = t.strip_prefix("name")
                && let Some((_, val)) = rest.split_once('=')
            {
                let name = val.trim().trim_matches('"').trim_matches('\'');
                if !name.is_empty() {
                    if in_bin {
                        return Some(name.to_owned());
                    } else if pkg_name.is_none() {
                        pkg_name = Some(name.to_owned());
                    }
                }
            }
        }
        if pkg_name.is_some() {
            return pkg_name;
        }
    }
    None
}

fn parse_opts_with_mode(args: &[String], cfg_test: bool) -> anyhow::Result<CliOpts> {
    let mut manifest = None;
    let mut bin_name = None;
    let mut pkg_name = None;
    let mut test_target = None;
    let mut crates = Vec::new();
    let mut exclude = Vec::new();
    let mut features = Vec::new();
    let mut all_features = false;
    let mut no_default_features = false;
    let mut target = None;
    let mut watch = false;
    let mut all_tests = false;
    let mut list_changed = false;
    let mut since = None;
    let mut junit = None;
    let mut json = false;
    let mut also_run = None;
    let mut retries = None;
    let mut explain = None;
    let mut backtest = false;
    let mut faults = 20usize;
    let mut full = false;
    let mut run_args = Vec::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--" => {
                run_args.extend(it.cloned());
                break;
            }
            "--manifest-path" => {
                manifest =
                    Some(PathBuf::from(it.next().context("--manifest-path requires a path")?));
            }
            "--bin" => {
                bin_name = Some(it.next().context("--bin requires a name")?.clone());
            }
            "--test" => {
                test_target =
                    Some(it.next().context("--test requires a test target name")?.clone());
            }
            "-p" | "--package" if cfg_test => {
                pkg_name = Some(it.next().context("-p requires a package name")?.clone());
            }
            "--crate" | "-p" | "--package" => {
                let val = it.next().context("--crate requires a crate name")?;
                if cfg_test && pkg_name.is_none() {
                    pkg_name = Some(val.clone());
                } else {
                    if pkg_name.is_none() {
                        pkg_name = Some(val.clone());
                    }
                    for c in val.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        crates.push(c.replace('-', "_"));
                    }
                }
            }
            "--exclude" => {
                let val = it.next().context("--exclude requires a package name")?;
                for ex in val.split([' ', ',']).map(str::trim).filter(|s| !s.is_empty()) {
                    if !exclude.iter().any(|x| x == ex) {
                        exclude.push(ex.to_owned());
                    }
                }
            }
            "--full" => full = true,
            "--workspace"
            | "--all-targets"
            | "--lib"
            | "--bins"
            | "--tests"
            | "--link-only"
            | "--cargo"
            | "--fast" => {}
            "-F" | "--features" => {
                let val = it.next().context("--features requires feature name(s)")?;
                for f in val.split([' ', ',']).map(str::trim).filter(|s| !s.is_empty()) {
                    if !features.iter().any(|x| x == f) {
                        features.push(f.to_owned());
                    }
                }
            }
            "--all-features" => all_features = true,
            "--no-default-features" => no_default_features = true,
            "--target" => {
                target = Some(it.next().context("--target requires a triple")?.clone());
            }
            "-w" | "--watch" => watch = true,
            "-c" | "--changed" => all_tests = false,
            "--list-changed" => list_changed = true,
            "--all" => all_tests = true,
            "--since" => {
                since = Some(it.next().context("--since requires a git revision/sha")?.clone());
            }
            "--junit" => {
                junit = Some(PathBuf::from(it.next().context("--junit requires a path")?));
            }
            "--json" => json = true,
            "--also-run" => {
                also_run =
                    Some(PathBuf::from(it.next().context("--also-run requires a file path")?));
            }
            "--retries" => {
                retries = Some(
                    it.next()
                        .context("--retries requires a count")?
                        .parse::<usize>()
                        .context("invalid --retries value")?,
                );
            }
            "--explain" => {
                explain = Some(it.next().context("--explain requires a test name")?.clone());
            }
            "--backtest" => backtest = true,
            "--faults" => {
                faults = it
                    .next()
                    .context("--faults requires a number")?
                    .parse::<usize>()
                    .context("invalid --faults value")?;
            }
            other if other.starts_with('-') => bail!("unknown option `{other}`"),
            positional if manifest.is_none() && Path::new(positional).exists() => {
                manifest = Some(PathBuf::from(positional));
            }
            filter if cfg_test => {
                run_args.push(filter.to_owned());
            }
            other => bail!("unexpected argument `{other}`"),
        }
    }

    let (manifest_path, workspace_root) = find_manifest_path(manifest)?;
    if cfg_test
        && pkg_name.is_none()
        && bin_name.is_none()
        && let Some(sha) = &since
    {
        let diff_files = crate::ci::analyze_git_diff_since(&workspace_root, sha)?;
        let inferred: Vec<String> =
            crate::ci::infer_packages_from_diff(&workspace_root, &diff_files)
                .into_iter()
                .filter(|p| !exclude.iter().any(|ex| ex == p))
                .collect();
        if let Some(first_pkg) = inferred.into_iter().next() {
            pkg_name = Some(first_pkg);
        }
    }
    let bin_name = if cfg_test {
        let pkg = pkg_name
            .clone()
            .or_else(|| bin_name.clone())
            .or_else(|| infer_default_bin(&workspace_root))
            .map(|p| p.strip_prefix("test-").unwrap_or(&p).replace('_', "-"))
            .context("could not infer package for `rustd test`; pass `-p <PACKAGE>`")?;
        if pkg_name.is_none() {
            pkg_name = Some(pkg.clone());
        }
        if let Some(tt) = &test_target {
            format!("test-{}", tt.replace('-', "_"))
        } else if let Some(b) = &bin_name {
            format!("test-bin-{}", b.replace('-', "_"))
        } else {
            format!("test-{}", pkg.replace('-', "_"))
        }
    } else {
        match bin_name
            .or_else(|| pkg_name.clone())
            .or_else(|| infer_default_bin(&workspace_root))
        {
            Some(b) => b,
            None => bail!("could not infer binary target; pass `--bin <NAME>` or `-p <PACKAGE>`"),
        }
    };
    Ok(CliOpts {
        manifest_path,
        workspace_root,
        bin_name,
        pkg_name,
        test_target,
        crates,
        exclude,
        features,
        all_features,
        no_default_features,
        target,
        watch,
        all_tests,
        list_changed,
        since,
        junit,
        json,
        also_run,
        retries,
        explain,
        backtest,
        faults,
        cfg_test,
        full,
        run_args,
    })
}

fn parse_opts(args: &[String]) -> anyhow::Result<CliOpts> {
    parse_opts_with_mode(args, false)
}

fn find_wild() -> Option<PathBuf> {
    if let Some(p) = env::var_os("WILD_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    if let Some(home) = env::var_os("HOME") {
        let cargo_wild = PathBuf::from(home).join(".cargo/bin/wild");
        if cargo_wild.exists() {
            return Some(cargo_wild);
        }
    }
    None
}

fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn quote_sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn collect_rlibs_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "rlib")
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("libcore-")
                        || n.starts_with("liballoc-")
                        || n.starts_with("libstd-")
                })
        })
        .collect()
}

fn sysroot_path(workspace_root: &Path) -> anyhow::Result<String> {
    let out = Command::new("rustc")
        .current_dir(workspace_root)
        .args(["--print", "sysroot"])
        .output()
        .context("running `rustc --print sysroot`")?;
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

fn sysroot_rlibs(workspace_root: &Path, target: Option<&str>) -> anyhow::Result<Vec<PathBuf>> {
    let sysroot = sysroot_path(workspace_root)?;
    let triple = match target {
        Some(t) => t.to_owned(),
        None => {
            let vv = Command::new("rustc")
                .current_dir(workspace_root)
                .arg("-vV")
                .output()
                .context("running `rustc -vV`")?;
            let vv_str = String::from_utf8(vv.stdout)?;
            vv_str
                .lines()
                .find_map(|l| l.strip_prefix("host: "))
                .context("missing host in rustc -vV")?
                .trim()
                .to_owned()
        }
    };
    Ok(collect_rlibs_in(&PathBuf::from(format!(
        "{sysroot}/lib/rustlib/{triple}/lib"
    ))))
}

fn apply_feature_args(cmd: &mut Command, opts: &CliOpts) {
    if !opts.features.is_empty() {
        cmd.arg("--features").arg(opts.features.join(","));
    }
    if opts.all_features {
        cmd.arg("--all-features");
    }
    if opts.no_default_features {
        cmd.arg("--no-default-features");
    }
}

fn features_fingerprint(opts: &CliOpts) -> String {
    format!(
        "features={} all={} nodefault={}\n",
        opts.features.join(","),
        opts.all_features,
        opts.no_default_features
    )
}

fn ensure_features_consistent(opts: &CliOpts, paths: &TargetPaths) {
    let cur_root = opts.workspace_root.display().to_string();
    let root_file = paths.dir.join("workspace.root");
    if let Ok(prev_root) = fs::read_to_string(&root_file) {
        let prev_root = prev_root.trim();
        if !prev_root.is_empty() && prev_root != cur_root {
            for fname in [
                "link_args.txt",
                "link.sh",
                "rlibs.txt",
                "clippy.sh",
                "clippy_tests.sh",
            ] {
                let fpath = paths.dir.join(fname);
                if let Ok(txt) = fs::read_to_string(&fpath)
                    && txt.contains(prev_root)
                {
                    let _ = fs::write(&fpath, txt.replace(prev_root, &cur_root));
                }
            }
            let _ = fs::remove_file(paths.dir.join("clippy.cache"));
            let _ = fs::remove_file(paths.dir.join("clippy_tests.cache"));
        }
    }
    let _ = fs::create_dir_all(&paths.dir);
    let _ = fs::write(&root_file, format!("{cur_root}\n"));
    let feat_file = paths.dir.join("features.key");
    let ws_feat_file = opts
        .workspace_root
        .join("target")
        .join("rustd")
        .join("features.key");
    let expected = features_fingerprint(opts);
    let prev_pkg_feat = fs::read_to_string(&feat_file).ok();
    let prev_ws_feat = fs::read_to_string(&ws_feat_file).ok();
    if prev_ws_feat.as_deref().is_some_and(|p| p != expected.as_str()) {
        stop_target_daemon(paths);
    }
    if prev_pkg_feat.as_deref() != Some(expected.as_str()) {
        if prev_pkg_feat.is_some() {
            let _ = fs::remove_file(&paths.link_args_txt);
            let _ = fs::remove_file(paths.dir.join("clippy.sh"));
            let _ = fs::remove_file(&paths.out_bin);
        }
        let _ = fs::create_dir_all(&paths.dir);
        let _ = fs::write(&feat_file, &expected);
        let _ = fs::write(&ws_feat_file, &expected);
    }
}

fn ensure_link_args(opts: &CliOpts, paths: &TargetPaths) -> anyhow::Result<String> {
    ensure_features_consistent(opts, paths);
    if let Ok(la_meta) = fs::metadata(&paths.link_args_txt)
        && let Ok(la_mtime) = la_meta.modified()
    {
        let manifest_newer = fs::metadata(&opts.manifest_path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t > la_mtime);
        let lock_newer = fs::metadata(opts.workspace_root.join("Cargo.lock"))
            .and_then(|m| m.modified())
            .is_ok_and(|t| t > la_mtime);
        if !manifest_newer
            && !lock_newer
            && let Ok(txt) = fs::read_to_string(&paths.link_args_txt)
        {
            let objs_exist = txt
                .lines()
                .rfind(|l| l.contains("-o") && (l.contains("cc") || l.contains("clang") || l.contains("rust-lld")))
                .is_some_and(|line| {
                    shell_words(line)
                        .into_iter()
                        .filter(|a| (a.ends_with(".rlib") || a.ends_with(".o")) && !a.contains("symbols.o"))
                        .all(|a| Path::new(&a).exists())
                });
            if objs_exist {
                return Ok(txt);
            }
        }
    }
    let debug_dir = opts.workspace_root.join("target").join("debug");
    if opts.cfg_test {
        let pkg = opts.pkg_name.as_deref().unwrap_or(&opts.bin_name);
        let krate = opts.test_target.as_deref().unwrap_or(pkg).replace('-', "_");
        eprintln!("Capturing test linker arguments via `cargo rustc --profile test -p {pkg}`...");
        if let Ok(entries) = fs::read_dir(debug_dir.join("deps")) {
            let prefix = format!("{krate}-");
            for e in entries.flatten() {
                let p = e.path();
                if let Some(name) = p.file_name().and_then(|n| n.to_str())
                    && name.starts_with(&prefix)
                    && p.extension().is_none()
                {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        let mut cmd = Command::new("cargo");
        cmd.current_dir(&opts.workspace_root).args(["rustc", "-p", pkg]);
        if let Some(tt) = &opts.test_target {
            cmd.args(["--test", tt]);
        } else if let Some(b) = opts.bin_name.strip_prefix("test-bin-") {
            cmd.args(["--bin", b]);
        } else {
            cmd.arg("--lib");
        }
        cmd.args(["--profile", "test"])
            .env("RUSTC_LOG", "rustc_codegen_ssa::back::link=info");
        apply_feature_args(&mut cmd, opts);
        if let Some(t) = &opts.target {
            cmd.args(["--target", t]);
        }
        cmd.args(["--", "--print", "link-args", "-C", "save-temps"]);
        let out = cmd.output().context("running cargo rustc --profile test to capture link args")?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        ensure!(
            combined
                .lines()
                .any(|l| l.contains("-o") && (l.contains("cc") || l.contains("clang"))),
            "cargo rustc --profile test did not emit a linker command for `{pkg}`:\n{}",
            combined
        );
        fs::write(&paths.link_args_txt, &combined)?;
        return Ok(combined);
    }
    eprintln!("Capturing linker arguments via `cargo rustc --bin {}`...", opts.bin_name);
    let _ = fs::remove_file(debug_dir.join(&opts.bin_name));
    if let Ok(entries) = fs::read_dir(debug_dir.join("deps")) {
        let prefix = format!("{}-", opts.bin_name.replace('-', "_"));
        for e in entries.flatten() {
            let p = e.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str())
                && name.starts_with(&prefix)
                && p.extension().is_none()
            {
                let _ = fs::remove_file(&p);
            }
        }
    }
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&opts.workspace_root);
    if let Some(pkg) = &opts.pkg_name {
        cmd.args(["rustc", "-p", pkg, "--bin", &opts.bin_name]);
    } else {
        cmd.args(["rustc", "--bin", &opts.bin_name]);
    }
    cmd.env("RUSTC_LOG", "rustc_codegen_ssa::back::link=info");
    apply_feature_args(&mut cmd, opts);
    if let Some(t) = &opts.target {
        cmd.args(["--target", t]);
    }
    cmd.args(["--", "--print", "link-args", "-C", "save-temps"]);
    let out = cmd.output().context("running cargo rustc to capture link args")?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    ensure!(
        combined.lines().any(|l| l.contains("-o")
            && (l.contains("cc") || l.contains("clang") || l.contains("rust-lld"))),
        "cargo build did not emit a linker command for `{}`",
        opts.bin_name
    );
    fs::write(&paths.link_args_txt, &combined)?;
    Ok(combined)
}

fn ensure_empty_objects(
    paths: &TargetPaths,
    target_triple: Option<&str>,
    force: bool,
) -> anyhow::Result<()> {
    let emits = match target_triple {
        Some(triple) => hir_ty::mir::codegen::Target::select(triple),
        None => hir_ty::mir::codegen::Target::selected(),
    }
    .map_err(anyhow::Error::msg)?;
    let _ = emits;
    if force || !paths.base_o.exists() {
        let empty = hir_ty::mir::codegen::empty_object().map_err(anyhow::Error::msg)?;
        fs::write(&paths.base_o, empty)?;
    }
    if force || !paths.patch_o.exists() {
        let empty = hir_ty::mir::codegen::empty_object().map_err(anyhow::Error::msg)?;
        fs::write(&paths.patch_o, empty)?;
    }
    Ok(())
}

/// Follows `harness/linkline.py::ahead` in upstream `rustd`:
/// - On macOS: weakens non-sysroot `.rlib` and `.rcgu.o` files via `weaken::weaken_file` and
///   inserts `[patch.o, base.o, "-Wl,-headerpad,0x200"]` ahead of rustc's objects.
/// - On Linux: inserts `[patch.o, base.o, "-Wl,--allow-multiple-definition"]` (and uses `wild`
///   when available).
fn prepare_link_script_and_symbols(
    opts: &CliOpts,
    paths: &TargetPaths,
) -> anyhow::Result<Vec<PathBuf>> {
    fs::create_dir_all(&paths.dir)?;
    ensure_empty_objects(paths, opts.target.as_deref(), !paths.out_bin.exists())?;
    let link_text = ensure_link_args(opts, paths)?;
    let sysroot = sysroot_path(&opts.workspace_root)?;
    let sys_rlibs = sysroot_rlibs(&opts.workspace_root, opts.target.as_deref())?;

    let line = link_text
        .lines()
        .rfind(|l| l.contains("-o") && (l.contains("cc") || l.contains("clang") || l.contains("rust-lld")))
        .context("no linker invocation found in link_args.txt")?;
    let words = shell_words(line);
    let idx = words
        .iter()
        .position(|w| !w.contains('=') && (w.ends_with("cc") || w.ends_with("clang") || w.contains("rust-lld")))
        .context("no compiler driver token in link line")?;
    let link_args: Vec<String> = words[idx..]
        .iter()
        .filter(|a| !a.ends_with("symbols.o"))
        .cloned()
        .collect();
    let first_obj = link_args
        .iter()
        .position(|a| a.ends_with(".rlib") || a.ends_with(".rcgu.o") || a.ends_with(".o"))
        .context("no .rlib/.rcgu.o in link args")?;

    let mut rlibs = Vec::new();
    for a in &link_args {
        if (a.ends_with(".rlib") || a.ends_with(".rcgu.o") || a.ends_with(".o"))
            && Path::new(a).exists()
        {
            rlibs.push(PathBuf::from(a));
        }
    }
    rlibs.extend(sys_rlibs.clone());

    let mut cmd_args = Vec::new();
    cmd_args.push(link_args[0].clone());

    if cfg!(target_os = "linux")
        && let Some(wild_bin) = find_wild()
    {
        let ld_wrapper = paths.dir.join("ld");
        let _ = fs::remove_file(&ld_wrapper);
        fs::write(
            &ld_wrapper,
            format!("#!/bin/sh\nexec {} \"$@\"\n", quote_sh(&wild_bin.display().to_string())),
        )?;
        fs::set_permissions(&ld_wrapper, fs::Permissions::from_mode(0o755))?;
        cmd_args.push(format!("-B{}", paths.dir.display()));
    }

    cmd_args.extend_from_slice(&link_args[1..first_obj]);
    cmd_args.push(paths.patch_o.display().to_string());
    cmd_args.push(paths.base_o.display().to_string());

    if cfg!(target_os = "macos") {
        cmd_args.push("-Wl,-headerpad,0x200".to_owned());
        cmd_args.push("-Wl,-w".to_owned());
        for arg in &link_args[first_obj..] {
            if (arg.ends_with(".rlib") || arg.ends_with(".rcgu.o"))
                && !arg.starts_with(&sysroot)
                && Path::new(arg).exists()
            {
                let weak_copy = crate::weaken::weaken_file(Path::new(arg))?;
                cmd_args.push(weak_copy.display().to_string());
            } else {
                cmd_args.push(arg.clone());
            }
        }
    } else {
        cmd_args.push("-Wl,--allow-multiple-definition".to_owned());
        cmd_args.extend_from_slice(&link_args[first_obj..]);
        for sr in &sys_rlibs {
            if let Some(fname) = sr.file_name().and_then(|n| n.to_str())
                && (fname.starts_with("libcore-") || fname.starts_with("liballoc-"))
            {
                cmd_args.push(format!(
                    "-Wl,--whole-archive,{},--no-whole-archive",
                    sr.display()
                ));
            }
        }
    }

    let mut cargo_out_bin = None;
    if let Some(o_pos) = cmd_args.iter().position(|x| x == "-o")
        && o_pos + 1 < cmd_args.len()
    {
        cargo_out_bin = Some(PathBuf::from(&cmd_args[o_pos + 1]));
        cmd_args[o_pos + 1] = paths.out_bin.display().to_string();
    }

    let sh = format!(
        "#!/bin/sh\nexec {}\n",
        cmd_args.iter().map(|s| quote_sh(s)).collect::<Vec<_>>().join(" ")
    );
    fs::write(&paths.link_sh, sh)?;
    fs::set_permissions(&paths.link_sh, fs::Permissions::from_mode(0o755))?;

    let rlibs_txt: String = rlibs.iter().map(|p| format!("{}\n", p.display())).collect();
    let _ = fs::write(&paths.rlibs_txt, rlibs_txt);

    // Ensure `paths.out_bin` is linked once with `-Wl,-headerpad,0x200` / `rustd-weak` so
    // `splice.rs` can splice into it immediately on the first edit.
    if !paths.out_bin.exists() {
        let linked = Command::new("sh")
            .arg("-c")
            .arg(&paths.link_sh)
            .status()
            .is_ok_and(|s| s.success());
        if !linked
            && let Some(cb) = cargo_out_bin
            && cb.exists()
        {
            let _ = fs::copy(&cb, &paths.out_bin);
        }
    }

    Ok(rlibs)
}

fn load_cached_rlibs(opts: &CliOpts, paths: &TargetPaths) -> anyhow::Result<Vec<PathBuf>> {
    if paths.link_sh.exists()
        && paths.rlibs_txt.exists()
        && paths.out_bin.exists()
        && let Ok(txt) = fs::read_to_string(&paths.rlibs_txt)
    {
        let list: Vec<PathBuf> = txt
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
        if !list.is_empty() && list.iter().all(|p| p.exists()) {
            ensure_empty_objects(paths, opts.target.as_deref(), false)?;
            return Ok(list);
        }
    }
    prepare_link_script_and_symbols(opts, paths)
}

fn send_unix_line(sock: &Path, line: &str) -> anyhow::Result<String> {
    let connect_path = env::current_dir()
        .ok()
        .and_then(|cwd| sock.strip_prefix(cwd).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| sock.to_path_buf());
    let mut stream = UnixStream::connect(&connect_path)?;
    stream.write_all(format!("{line}\n").as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply.trim().to_owned())
}

fn target_request_line(
    req_kind: &str,
    opts: &CliOpts,
    paths: &TargetPaths,
    bin_crate: &str,
    rlibs: &[PathBuf],
) -> String {
    let target = crate::Target {
        project: Some(opts.manifest_path.clone()),
        krate: bin_crate.to_owned(),
        out: paths.base_o.clone(),
        patch: Some(paths.patch_o.clone()),
        link_cmd: Some(paths.link_sh.display().to_string()),
        splice: Some(paths.out_bin.clone()),
        symbol_files: rlibs.to_vec(),
    };
    format!("{req_kind}{}", target.fields())
}

fn ensure_daemon(
    opts: &CliOpts,
    paths: &TargetPaths,
    req_kind: &str,
) -> anyhow::Result<Option<String>> {
    ensure_features_consistent(opts, paths);
    let bin_crate = opts
        .test_target
        .as_deref()
        .or(opts.pkg_name.as_deref())
        .unwrap_or(&opts.bin_name)
        .replace('-', "_");
    let rlibs = load_cached_rlibs(opts, paths)?;

    if paths.rustd_sock.exists() {
        let req = target_request_line(req_kind, opts, paths, &bin_crate, &rlibs);
        if let Ok(reply) = send_unix_line(&paths.rustd_sock, &req) {
            if !reply.starts_with("error: unknown request") {
                return Ok(Some(reply));
            }
        }
        stop_target_daemon(paths);
    }

    let _ = fs::remove_file(&paths.rustd_sock);
    fs::create_dir_all(&paths.store_dir)?;

    let exe = env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.current_dir(&opts.workspace_root).arg(&opts.manifest_path);
    if let Some(t) = &opts.target {
        cmd.args(["--target", t]);
    }
    cmd.args(["--crate", &bin_crate]);
    for c in &opts.crates {
        if c != &bin_crate {
            cmd.args(["--crate", c]);
        }
    }
    if opts.cfg_test {
        cmd.arg("--cfg-test").arg("--dev-deps");
    }
    if !opts.features.is_empty() {
        cmd.args(["--features", &opts.features.join(",")]);
    }
    cmd.arg("--out")
        .arg(&paths.base_o)
        .arg("--patch")
        .arg(&paths.patch_o)
        .arg("--socket")
        .arg(&paths.rustd_sock)
        .arg("--link-cmd")
        .arg(&paths.link_sh)
        .arg("--binary")
        .arg(&paths.out_bin)
        .env("RUSTD_BIN_CRATE", &bin_crate)
        .env("RUSTD_CACHE", &paths.store_dir)
        .env("RUSTD_SKIP_COLD_LINK", "1");
    if !opts.full && env::var_os("RUSTD_ALONE").is_none() {
        cmd.env("RUSTD_INCREMENTAL_ONLY", "1");
    }
    for r in &rlibs {
        cmd.arg("--symbols").arg(r);
    }

    eprintln!(
        "[rustd] starting daemon for `{}` (crate `{bin_crate}`)...",
        opts.bin_name,
    );
    let t0 = Instant::now();
    let log = fs::File::create(&paths.rustd_log)?;
    let log_err = log.try_clone()?;
    let mut child = cmd
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .context("spawning rustd daemon")?;

    while !paths.rustd_sock.exists() {
        if let Some(status) = child.try_wait()? {
            let tail = fs::read_to_string(&paths.rustd_log).unwrap_or_default();
            bail!("rustd daemon exited early ({status}):\n{tail}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    fs::write(&paths.pid_file, format!("{}\n", child.id()))?;
    eprintln!(
        "[rustd] ready in {:.1?} (daemon pid={}, binary={})",
        t0.elapsed(),
        child.id(),
        paths.out_bin.display()
    );
    let req = target_request_line(req_kind, opts, paths, &bin_crate, &rlibs);
    let reply = send_unix_line(&paths.rustd_sock, &req).ok();
    Ok(reply)
}

fn check_compile_error(_opts: &CliOpts, _paths: &TargetPaths, reply: Option<&str>) -> bool {
    let Some(r) = reply else { return false };
    if !r.starts_with("error:") {
        return false;
    }
    eprintln!(
        "\x1b[1;31merror\x1b[0m\x1b[1m: {}\x1b[0m",
        r.trim_start_matches("error:").trim()
    );
    true
}

fn cmd_check(args: &[String]) -> anyhow::Result<()> {
    let has_test_flag = args
        .iter()
        .any(|a| a == "-p" || a == "--package" || a == "--test" || a == "--tests" || a == "--since");
    let mut opts = parse_opts_with_mode(args, has_test_flag)?;
    if opts.bin_name.starts_with("test-") {
        opts.cfg_test = true;
    }
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    let t0 = Instant::now();
    let reply = ensure_daemon(&opts, &paths, "patch")?;
    let elapsed = t0.elapsed();
    if check_compile_error(&opts, &paths, reply.as_deref()) {
        std::process::exit(101);
    }
    eprintln!(
        "    \x1b[1;32mFinished\x1b[0m `dev` profile [unoptimized + debuginfo] target(s) in {:.2}s ({:.1?})",
        elapsed.as_secs_f64(),
        elapsed
    );
    Ok(())
}

fn ensure_clippy_script(
    opts: &CliOpts,
    paths: &TargetPaths,
    with_tests: bool,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(&paths.dir)?;
    ensure_features_consistent(opts, paths);
    let script_name = if with_tests { "clippy_tests.sh" } else { "clippy.sh" };
    let clippy_sh = paths.dir.join(script_name);
    if let Ok(meta) = fs::metadata(&clippy_sh)
        && let Ok(mtime) = meta.modified()
    {
        let manifest_newer = fs::metadata(&opts.manifest_path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t > mtime);
        let lock_newer = fs::metadata(opts.workspace_root.join("Cargo.lock"))
            .and_then(|m| m.modified())
            .is_ok_and(|t| t > mtime);
        if !manifest_newer
            && !lock_newer
            && let Ok(txt) = fs::read_to_string(&clippy_sh)
        {
            let deps_exist = shell_words(&txt)
                .into_iter()
                .filter_map(|w| w.split_once('=').map(|(_, r)| r.to_owned()))
                .filter(|p| {
                    p.ends_with(".rmeta")
                        || p.ends_with(".rlib")
                        || p.ends_with(".dylib")
                        || p.ends_with(".so")
                })
                .all(|p| Path::new(&p).exists());
            if deps_exist {
                return Ok(clippy_sh);
            }
        }
    }
    let pkg = opts.pkg_name.as_deref().unwrap_or(&opts.bin_name);
    let krate = pkg.replace('-', "_");
    let deps_dir = opts.workspace_root.join("target").join("debug").join("deps");
    if let Ok(entries) = fs::read_dir(&deps_dir) {
        let prefix1 = format!("{krate}-");
        let prefix2 = format!("lib{krate}-");
        for e in entries.flatten() {
            let p = e.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str())
                && (name.starts_with(&prefix1) || name.starts_with(&prefix2))
                && p.extension().is_some_and(|ext| ext == "rmeta")
            {
                let _ = fs::remove_file(&p);
            }
        }
    }
    eprintln!(
        "[rustd] capturing real `clippy-driver` invocation via `cargo clippy -p {pkg} --no-deps`..."
    );
    let mut clippy_cmd = Command::new("cargo");
    clippy_cmd
        .current_dir(&opts.workspace_root)
        .args(["clippy", "-p", pkg]);
    if with_tests {
        clippy_cmd.args(["--profile", "test"]);
    }
    clippy_cmd
        .args(["--no-deps", "-vv"])
        .env("CARGO_INCREMENTAL", "1")
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "");
    apply_feature_args(&mut clippy_cmd, opts);
    let out = clippy_cmd
        .output()
        .context("running `cargo clippy -vv`")?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let crate_flag = format!("--crate-name {krate} ");
    let running_line = stderr
        .lines()
        .rfind(|l| l.contains("Running `") && l.contains("clippy-driver") && l.contains(&crate_flag))
        .or_else(|| {
            stderr
                .lines()
                .rfind(|l| l.contains("Running `") && l.contains("clippy-driver"))
        })
        .context("could not find `clippy-driver` invocation in `cargo clippy -vv` output")?;
    let raw_cmd = running_line
        .split_once("Running `")
        .and_then(|(_, rest)| rest.rsplit_once('`'))
        .map(|(c, _)| c)
        .context("malformed Running line in cargo clippy output")?;

    let words = shell_words(raw_cmd);
    let mut env_pairs = Vec::new();
    let mut argv = Vec::new();
    let mut in_env = true;
    let mut saw_incr = false;
    let incr_dir = paths.dir.join(if with_tests {
        "clippy-incr-tests"
    } else {
        "clippy-incr"
    });
    for w in words {
        if in_env && w.contains('=') && !w.starts_with('-') {
            env_pairs.push(w);
            continue;
        }
        in_env = false;
        if w.starts_with("--error-format=") || w.starts_with("--json=") {
            continue;
        }
        if w == "-C" {
            argv.push(w);
            continue;
        }
        if w.starts_with("incremental=") {
            argv.push(format!("incremental={}", incr_dir.display()));
            saw_incr = true;
            continue;
        }
        if w.starts_with("-Cincremental=") {
            argv.push("-C".to_owned());
            argv.push(format!("incremental={}", incr_dir.display()));
            saw_incr = true;
            continue;
        }
        argv.push(w);
    }
    if !saw_incr {
        argv.push("-C".to_owned());
        argv.push(format!("incremental={}", incr_dir.display()));
    }
    argv.push("--remap-path-prefix".to_owned());
    argv.push(format!("{}=.", opts.workspace_root.display()));
    argv.push("--color=always".to_owned());

    let mut script = String::from("#!/bin/sh\n");
    for ep in &env_pairs {
        if let Some((k, v)) = ep.split_once('=') {
            script.push_str(&format!("export {}={}\n", k, quote_sh(v)));
        }
    }
    script.push_str(&format!(
        "exec {} \"$@\"\n",
        argv.iter().map(|a| quote_sh(a)).collect::<Vec<_>>().join(" ")
    ));
    fs::write(&clippy_sh, script)?;
    fs::set_permissions(&clippy_sh, fs::Permissions::from_mode(0o755))?;
    Ok(clippy_sh)
}

fn workspace_source_fingerprint(workspace_root: &Path, extra: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut files = Vec::new();
    let mut stack = vec![workspace_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if !p.ends_with("target") && !p.ends_with(".git") {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|ext| ext == "rs" || ext == "toml")
                && let Ok(m) = p.metadata()
            {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                files.push((p, m.len(), mtime));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = rustc_hash::FxHasher::default();
    extra.hash(&mut h);
    for (p, len, mtime) in files {
        p.hash(&mut h);
        len.hash(&mut h);
        mtime.hash(&mut h);
    }
    h.finish()
}

fn cmd_clippy(args: &[String]) -> anyhow::Result<()> {
    let with_tests = args.iter().any(|a| a == "--tests" || a == "--test");
    let mut opts = parse_opts_with_mode(args, true)?;
    opts.cfg_test = true;
    if opts
        .pkg_name
        .as_ref()
        .is_some_and(|p| opts.exclude.iter().any(|ex| ex == p))
    {
        eprintln!("[rustd] package excluded by --exclude; skipping clippy");
        return Ok(());
    }
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    let clippy_sh = ensure_clippy_script(&opts, &paths, with_tests)?;
    let clippy_cache = paths.dir.join(if with_tests {
        "clippy_tests.cache"
    } else {
        "clippy.cache"
    });
    let deny_warnings = opts.run_args.windows(2).any(|w| (w[0] == "-D" || w[0] == "--deny") && w[1] == "warnings")
        || opts.run_args.iter().any(|a| a == "-Dwarnings" || a == "--deny=warnings");
    let filtered_args: Vec<String> = {
        let mut out = Vec::new();
        let mut i = 0;
        while i < opts.run_args.len() {
            if (opts.run_args[i] == "-D" || opts.run_args[i] == "--deny")
                && opts.run_args.get(i + 1).is_some_and(|v| v == "warnings")
            {
                out.push("-W".to_owned());
                out.push("warnings".to_owned());
                i += 2;
                continue;
            }
            if opts.run_args[i] == "-Dwarnings" || opts.run_args[i] == "--deny=warnings" {
                out.push("-Wwarnings".to_owned());
                i += 1;
                continue;
            }
            out.push(opts.run_args[i].clone());
            i += 1;
        }
        out
    };
    let run_once = || -> anyhow::Result<bool> {
        let t0 = Instant::now();
        let extra_key = format!("{}:{:?}:{deny_warnings}", clippy_sh.display(), opts.run_args);
        let fp = workspace_source_fingerprint(&opts.workspace_root, &extra_key);
        let fp_prefix = format!("{fp:016x} ");
        if let Ok(cached) = fs::read_to_string(&clippy_cache)
            && let Some((first_line, rest)) = cached.split_once('\n')
            && let Some(status_str) = first_line.strip_prefix(&fp_prefix)
        {
            let ok = status_str.trim() == "1";
            if !rest.is_empty() {
                eprint!("{rest}");
            }
            if ok {
                let elapsed = t0.elapsed();
                eprintln!(
                    "    \x1b[1;32mFinished\x1b[0m `dev` profile [unoptimized + debuginfo] target(s) in {:.2}s ({:.1?})",
                    elapsed.as_secs_f64(),
                    elapsed
                );
            }
            return Ok(ok);
        }
        let out = Command::new(&clippy_sh)
            .current_dir(&opts.workspace_root)
            .args(&filtered_args)
            .output()
            .context("running cached clippy-driver")?;
        let elapsed = t0.elapsed();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let has_warning = stderr.contains("warning:") && !stderr.contains("0 warnings emitted");
        let ok = out.status.success() && (!deny_warnings || !has_warning);
        let _ = fs::write(
            &clippy_cache,
            format!("{fp:016x} {}\n{stderr}", if ok { 1 } else { 0 }),
        );
        if !stderr.is_empty() {
            eprint!("{stderr}");
        }
        if ok {
            eprintln!(
                "    \x1b[1;32mFinished\x1b[0m `dev` profile [unoptimized + debuginfo] target(s) in {:.2}s ({:.1?})",
                elapsed.as_secs_f64(),
                elapsed
            );
        }
        Ok(ok)
    };
    let ok = run_once()?;
    if !ok {
        std::process::exit(101);
    }
    Ok(())
}

fn cmd_fmt(args: &[String]) -> anyhow::Result<()> {
    let check_only = args.iter().any(|a| a == "--check");
    let mut since = None;
    let mut manifest = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--check" => {}
            "--since" => {
                since = Some(it.next().context("--since requires a git revision/sha")?.clone());
            }
            "--manifest-path" => {
                manifest = Some(PathBuf::from(
                    it.next().context("--manifest-path requires a path")?,
                ));
            }
            _ => {}
        }
    }
    let (_, workspace_root) = find_manifest_path(manifest)?;
    let t0 = Instant::now();
    let sha = since.as_deref().unwrap_or("HEAD");
    let mut rs_files = Vec::new();
    if let Ok(out) = Command::new("git")
        .current_dir(&workspace_root)
        .args(["diff", "--name-only", sha])
        .output()
        && out.status.success()
    {
        for l in String::from_utf8_lossy(&out.stdout).lines() {
            let p = l.trim();
            if p.ends_with(".rs") && workspace_root.join(p).exists() {
                rs_files.push(p.to_owned());
            }
        }
    }
    if let Ok(out) = Command::new("git")
        .current_dir(&workspace_root)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        && out.status.success()
    {
        for l in String::from_utf8_lossy(&out.stdout).lines() {
            let p = l.trim();
            if p.ends_with(".rs")
                && !rs_files.iter().any(|x| x == p)
                && workspace_root.join(p).exists()
            {
                rs_files.push(p.to_owned());
            }
        }
    }
    if rs_files.is_empty() {
        eprintln!(
            "    \x1b[1;32mFinished\x1b[0m `rustfmt` (0 modified .rs files) in {:.1?}",
            t0.elapsed()
        );
        return Ok(());
    }
    let rustfmt_bin = env::var_os("HOME")
        .and_then(|h| {
            let tc_dir = PathBuf::from(h).join(".rustup").join("toolchains");
            fs::read_dir(tc_dir).ok()
        })
        .and_then(|entries| {
            entries
                .flatten()
                .find(|e| e.file_name().to_string_lossy().starts_with("nightly-"))
                .map(|e| e.path().join("bin").join("rustfmt"))
                .filter(|p| p.exists())
        })
        .unwrap_or_else(|| PathBuf::from("rustfmt"));
    let mut cmd = Command::new(rustfmt_bin);
    cmd.current_dir(&workspace_root).arg("--edition=2024");
    if check_only {
        cmd.arg("--check");
    }
    cmd.args(&rs_files);
    let status = cmd.status().context("running `rustfmt`")?;
    eprintln!(
        "    \x1b[1;32mFinished\x1b[0m `rustfmt` ({} file(s)) in {:.1?}",
        rs_files.len(),
        t0.elapsed()
    );
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn cmd_build(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_opts(args)?;
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    let reply = ensure_daemon(&opts, &paths, "sync")?;
    if check_compile_error(&opts, &paths, reply.as_deref()) {
        std::process::exit(101);
    }
    println!("{}", paths.out_bin.display());
    Ok(())
}

fn cmd_run(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_opts(args)?;
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    let reply = ensure_daemon(&opts, &paths, "sync")?;
    if check_compile_error(&opts, &paths, reply.as_deref()) {
        std::process::exit(101);
    }
    let mut cmd = Command::new(&paths.out_bin);
    cmd.args(&opts.run_args);
    let err = cmd.exec();
    Err(err.into())
}

fn resolve_test_args(opts: &CliOpts, paths: &TargetPaths) -> Vec<String> {
    if !opts.run_args.is_empty() || opts.all_tests {
        return opts.run_args.clone();
    }
    let tests_txt = paths.base_o.with_extension("tests.txt");
    if let Ok(txt) = fs::read_to_string(tests_txt) {
        let impacted: Vec<String> = txt
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with("@target="))
            .map(str::to_owned)
            .collect();
        if !impacted.is_empty() && impacted.len() <= 64 {
            let preview: Vec<&str> = impacted.iter().take(5).map(String::as_str).collect();
            let suffix = if impacted.len() > 5 {
                format!(", ... (+{} more)", impacted.len() - 5)
            } else {
                String::new()
            };
            eprintln!(
                "[rustd] running {} impacted test(s): {}{suffix}",
                impacted.len(),
                preview.join(", ")
            );
            return impacted;
        }
    }
    opts.run_args.clone()
}

fn ensure_integration_test_bin(
    opts: &CliOpts,
    lib_paths: &TargetPaths,
    target_krate: &str,
) -> anyhow::Result<PathBuf> {
    let mut t_opts = opts.clone();
    t_opts.test_target = Some(target_krate.to_owned());
    t_opts.bin_name = format!("{}-{target_krate}", opts.bin_name);
    let t_paths = TargetPaths::new(&opts.workspace_root, &t_opts.bin_name);
    if !t_paths.out_bin.exists() {
        let _ = prepare_link_script_and_symbols(&t_opts, &t_paths)?;
        let _ = fs::copy(&lib_paths.base_o, &t_paths.base_o);
        let _ = fs::copy(&lib_paths.patch_o, &t_paths.patch_o);
        let linked_ok = Command::new("sh")
            .arg("-c")
            .arg(&t_paths.link_sh)
            .status()
            .is_ok_and(|s| s.success());
        if !linked_ok {
            let deps_dir = opts.workspace_root.join("target").join("debug").join("deps");
            let prefix = format!("{}-", target_krate.replace('-', "_"));
            let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
            if let Ok(entries) = fs::read_dir(&deps_dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if let Some(name) = p.file_name().and_then(|n| n.to_str())
                        && name.starts_with(&prefix)
                        && p.extension().is_none()
                        && let Ok(m) = p.metadata()
                        && m.is_file()
                        && (m.permissions().mode() & 0o111 != 0)
                        && let Ok(mtime) = m.modified()
                    {
                        if best.as_ref().is_none_or(|(bm, _)| mtime > *bm) {
                            best = Some((mtime, p));
                        }
                    }
                }
            }
            if let Some((_, cargo_bin)) = best {
                fs::copy(&cargo_bin, &t_paths.out_bin)?;
            } else {
                bail!("linking integration test target `{target_krate}` failed");
            }
        }
    }
    Ok(t_paths.out_bin)
}

fn find_package_dir(workspace_root: &Path, pkg_name: &str) -> Option<PathBuf> {
    let mut stack = vec![workspace_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists()
            && let Ok(txt) = fs::read_to_string(&manifest)
        {
            let mut in_pkg = false;
            for line in txt.lines() {
                let t = line.trim();
                if t.starts_with('[') {
                    in_pkg = t == "[package]";
                    continue;
                }
                if in_pkg
                    && let Some(rest) = t.strip_prefix("name")
                    && let Some((_, val)) = rest.split_once('=')
                {
                    let name = val.trim().trim_matches('"').trim_matches('\'');
                    if name == pkg_name {
                        return Some(dir);
                    }
                    break;
                }
            }
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && !p.ends_with("target") && !p.ends_with(".git") {
                stack.push(p);
            }
        }
    }
    None
}

fn stop_target_daemon(paths: &TargetPaths) {
    if let Ok(pid_str) = fs::read_to_string(&paths.pid_file)
        && let Ok(pid) = pid_str.trim().parse::<u32>()
        && pid > 0
    {
        let _ = Command::new("kill").arg("-15").arg(pid.to_string()).status();
    }
    let _ = fs::remove_file(&paths.rustd_sock);
    let _ = fs::remove_file(&paths.pid_file);
}

fn cmd_start(args: &[String]) -> anyhow::Result<()> {
    let has_test_flag = args
        .iter()
        .any(|a| a == "-p" || a == "--package" || a == "--test" || a == "--tests");
    let mut opts = parse_opts_with_mode(args, has_test_flag)?;
    if opts.bin_name.starts_with("test-") {
        opts.cfg_test = true;
    }
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    ensure_daemon(&opts, &paths, "sync")?;
    let pid = fs::read_to_string(&paths.pid_file).unwrap_or_default();
    eprintln!(
        "[rustd] daemon warm and ready: bin={} pid={} socket={}",
        opts.bin_name,
        pid.trim(),
        paths.rustd_sock.display()
    );
    Ok(())
}

fn cmd_test(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_opts_with_mode(args, true)?;
    if opts
        .pkg_name
        .as_ref()
        .is_some_and(|p| opts.exclude.iter().any(|ex| ex == p))
    {
        if let Some(junit_path) = &opts.junit {
            crate::ci::write_junit_xml(junit_path, &[], Duration::ZERO)?;
        }
        if opts.json {
            crate::ci::print_json_summary(&[], Duration::ZERO);
        } else {
            eprintln!("[rustd] package excluded by --exclude; 0 tests to run");
        }
        return Ok(());
    }
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    let baseline_cache = paths.base_o.with_extension("baseline.cache");

    if let Some(sha) = &opts.since {
        let diff_files = crate::ci::analyze_git_diff_since(&opts.workspace_root, sha)?;
        fs::create_dir_all(&paths.dir)?;
        crate::ci::write_since_baseline_cache(
            &baseline_cache,
            &format!("SINCE:{sha}"),
            &diff_files,
        )?;
        let _ = fs::remove_file(paths.base_o.with_extension("tests.txt"));
    } else if baseline_cache.exists() {
        let _ = fs::remove_file(&baseline_cache);
        let _ = fs::remove_file(paths.base_o.with_extension("tests.txt"));
    }

    let reply = ensure_daemon(&opts, &paths, "sync")?;

    if opts.backtest {
        let bt_reply =
            send_unix_line(&paths.rustd_sock, &format!("backtest {}", opts.faults))?;
        ensure!(!bt_reply.starts_with("error:"), "rustd backtest: {bt_reply}");
        let bt_txt = paths.base_o.with_extension("backtest.txt");
        if let Ok(report) = fs::read_to_string(&bt_txt) {
            print!("{report}");
        }
        let mut caught = 0usize;
        let mut total = 0usize;
        for w in bt_reply.split_whitespace() {
            if let Some(v) = w.strip_prefix("caught=") {
                caught = v.parse().unwrap_or(0);
            } else if let Some(v) = w.strip_prefix("total=") {
                total = v.parse().unwrap_or(0);
            }
        }
        if total > 0 && caught * 100 < total * 99 {
            std::process::exit(1);
        }
        return Ok(());
    }

    if let Some(query) = &opts.explain {
        let explain_txt = paths.base_o.with_extension("explain.txt");
        let mut matched = false;
        if let Ok(txt) = fs::read_to_string(&explain_txt) {
            for line in txt.lines() {
                if let Some((test_name, why)) = line.split_once('\t')
                    && (test_name == query || test_name.contains(query.as_str()))
                {
                    println!("explain `{test_name}`:");
                    for (idx, step) in why.split(" <- ").enumerate() {
                        if idx == 0 {
                            println!("  {test_name}");
                            println!("  <- {step}");
                        } else {
                            println!("  <- {step}");
                        }
                    }
                    matched = true;
                }
            }
        }
        if !matched {
            println!("explain `{query}`: not selected (no path from modified items to `{query}`)");
        }
        return Ok(());
    }

    if opts.list_changed {
        let tests_txt = paths.base_o.with_extension("tests.txt");
        if let Ok(txt) = fs::read_to_string(tests_txt) {
            for line in txt.lines().map(str::trim).filter(|s| !s.is_empty()) {
                if let Some(rest) = line.strip_prefix("@target=")
                    && let Some((tk, rel)) = rest.split_once('\t')
                {
                    println!("{tk}::{rel}");
                } else {
                    println!("{line}");
                }
            }
        }
        return Ok(());
    }

    let is_ci_runner = opts.since.is_some()
        || opts.junit.is_some()
        || opts.json
        || opts.also_run.is_some()
        || opts.retries.is_some();

    if is_ci_runner && !opts.watch {
        if check_compile_error(&opts, &paths, reply.as_deref()) {
            std::process::exit(101);
        }
        let lib_only = args.iter().any(|a| a == "--lib");
        let tests_txt = paths.base_o.with_extension("tests.txt");
        let mut raw_wanted: Vec<String> = fs::read_to_string(&tests_txt)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if let Some(also_path) = &opts.also_run
            && let Ok(also_txt) = fs::read_to_string(also_path)
        {
            for line in also_txt.lines().map(str::trim).filter(|s| !s.is_empty()) {
                if !raw_wanted.iter().any(|w| w == line) {
                    raw_wanted.push(line.to_owned());
                }
            }
        }
        let mut lib_wanted = Vec::new();
        let mut integration_wanted: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        let pkg_dir = opts
            .pkg_name
            .as_deref()
            .and_then(|p| find_package_dir(&opts.workspace_root, p))
            .or_else(|| opts.manifest_path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| opts.workspace_root.clone());
        for w in raw_wanted {
            if let Some(rest) = w.strip_prefix("@target=")
                && let Some((tk, rel)) = rest.split_once('\t')
            {
                if !lib_only {
                    let list = integration_wanted.entry(tk.to_owned()).or_default();
                    if !list.iter().any(|x| x == rel) {
                        list.push(rel.to_owned());
                    }
                }
            } else if let Some((tk, rel)) = w.split_once("::")
                && (pkg_dir.join("tests").join(tk).exists()
                    || pkg_dir.join("tests").join(format!("{tk}.rs")).exists())
            {
                if !lib_only {
                    let list = integration_wanted.entry(tk.to_owned()).or_default();
                    if !list.iter().any(|x| x == rel) {
                        list.push(rel.to_owned());
                    }
                }
            } else if !lib_wanted.contains(&w) {
                lib_wanted.push(w);
            }
        }
        let explain_txt = paths.base_o.with_extension("explain.txt");
        let mut reasons = std::collections::HashMap::new();
        if let Ok(txt) = fs::read_to_string(&explain_txt) {
            for line in txt.lines() {
                if let Some((t, r)) = line.split_once('\t') {
                    reasons.insert(t.to_owned(), r.to_owned());
                }
            }
        }
        let exact_lib_tests = crate::ci::expand_exact_tests(&paths.out_bin, &lib_wanted);
        let mut target_runs: Vec<(String, PathBuf, Vec<String>)> = Vec::new();
        let raw_pkg = opts.pkg_name.as_deref().unwrap_or(&opts.bin_name);
        let pkg = raw_pkg.strip_prefix("test-").unwrap_or(raw_pkg);
        if !exact_lib_tests.is_empty() {
            target_runs.push((pkg.to_owned(), paths.out_bin.clone(), exact_lib_tests));
        }
        for (tk, tk_wanted) in integration_wanted {
            if tk_wanted.is_empty() {
                continue;
            }
            let tk_bin = ensure_integration_test_bin(&opts, &paths, &tk)?;
            let exact_tk = crate::ci::expand_exact_tests(&tk_bin, &tk_wanted);
            if !exact_tk.is_empty() {
                target_runs.push((format!("{pkg}::{tk}"), tk_bin, exact_tk));
            }
        }
        let total_tests: usize = target_runs.iter().map(|(_, _, t)| t.len()).sum();
        let cfg = crate::ci::NextestConfig::load(&opts.workspace_root, opts.retries);
        let t0 = Instant::now();
        if !opts.json {
            eprintln!(
                "    \x1b[1;32mStarting\x1b[0m {} impacted test(s) across {} binary(s) (isolated processes)",
                total_tests,
                target_runs.len().max(1)
            );
        }
        let mut outcomes = Vec::with_capacity(total_tests);
        for (run_pkg, run_bin, run_tests) in &target_runs {
            outcomes.extend(crate::ci::run_isolated_test_suite(
                run_pkg,
                run_bin,
                run_tests,
                &reasons,
                &cfg,
                &opts.run_args,
                opts.json,
            ));
        }
        let total_dur = t0.elapsed();
        let zero_matched_count = outcomes.iter().filter(|o| o.zero_matched).count();
        let passed = outcomes.iter().filter(|o| o.passed).count();
        let failed = outcomes
            .iter()
            .filter(|o| !o.passed && !o.zero_matched)
            .count();
        if let Some(junit_path) = &opts.junit {
            crate::ci::write_junit_xml(junit_path, &outcomes, total_dur)?;
        }
        if opts.json {
            crate::ci::print_json_summary(&outcomes, total_dur);
        } else {
            eprintln!(
                "------------\n     \x1b[1;32mSummary\x1b[0m [{:>7.3}s] {} tests run: {} passed, {} failed{}",
                total_dur.as_secs_f64(),
                outcomes.len(),
                passed,
                failed,
                if zero_matched_count > 0 {
                    format!(", {zero_matched_count} unmatched (runner error)")
                } else {
                    String::new()
                }
            );
        }
        if zero_matched_count > 0 {
            std::process::exit(2);
        }
        if failed > 0 {
            std::process::exit(101);
        }
        return Ok(());
    }

    if check_compile_error(&opts, &paths, reply.as_deref()) {
        std::process::exit(101);
    }
    let test_args = resolve_test_args(&opts, &paths);
    let mut cmd = Command::new(&paths.out_bin);
    cmd.args(&test_args);
    let err = cmd.exec();
    Err(err.into())
}

fn cmd_status(args: &[String]) -> anyhow::Result<()> {
    let (_, workspace_root) = find_manifest_path(
        args.windows(2)
            .find(|w| w[0] == "--manifest-path")
            .map(|w| PathBuf::from(&w[1])),
    )?;
    let paths = TargetPaths::new(&workspace_root, "default");
    if paths.rustd_sock.exists() {
        let pid = fs::read_to_string(&paths.pid_file).unwrap_or_default();
        println!(
            "running: pid={} socket={}",
            pid.trim(),
            paths.rustd_sock.display()
        );
    } else {
        println!("stopped");
    }
    Ok(())
}

fn cmd_stop(args: &[String]) -> anyhow::Result<()> {
    let (_, workspace_root) = find_manifest_path(
        args.windows(2)
            .find(|w| w[0] == "--manifest-path")
            .map(|w| PathBuf::from(&w[1])),
    )?;
    let paths = TargetPaths::new(&workspace_root, "default");
    stop_target_daemon(&paths);
    println!("stopped");
    Ok(())
}
