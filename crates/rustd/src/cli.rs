//! High-level `rustd` Cargo-style CLI (`rustd build`, `rustd run`, `rustd patch`, `rustd sync`,
//! `rustd status`, `rustd stop`).
//!
//! Works on any Cargo workspace by capturing the `rustc` linker invocation into
//! `<target_dir>/rustd/<bin>/link_args.txt`, linking with `wild --allow-multiple-definition`,
//! and keeping a background `rustd --watch` daemon warm so edits hot-patch running binaries
//! in-place via `hotpatch_rt.rs` (~0.2–4 ms) and subsequent `rustd run` invocations start in ~7 ms.

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
            | "patch"
            | "sync"
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
        "build" => cmd_build(&args),
        "check" => cmd_check(&args, "check"),
        "clippy" => cmd_clippy(&args),
        "fmt" => cmd_fmt(&args),
        "rebase" => cmd_rebase(&args),
        "run" => cmd_run(&args),
        "test" => cmd_test(&args),
        "patch" => cmd_request("patch", &args),
        "sync" => cmd_request("sync", &args),
        "start" | "warm" => cmd_start(&args),
        "status" => cmd_status(&args),
        "stop" => cmd_stop(&args, false),
        _ => {
            eprintln!(
                "Usage:\n  \
                 rustd check  [-p PACKAGE] [--bin NAME] [--test NAME] [--watch]\n  \
                 rustd clippy [-p PACKAGE] [--since SHA] [--watch] [-- ARGS...]\n  \
                 rustd fmt    [--check] [--since SHA]\n  \
                 rustd rebase [-p PACKAGE] [--bin NAME] [--test NAME]\n  \
                 rustd build  [--manifest-path PATH] [--bin NAME] [--crate CRATE]... [--target TRIPLE] [--watch]\n  \
                 rustd run    [--manifest-path PATH] [--bin NAME] [--crate CRATE]... [--target TRIPLE] [-- ARGS...]\n  \
                 rustd test   [FILTER] [-p PACKAGE] [--since SHA] [--junit PATH] [--json] [--also-run FILE] [--retries N] [--explain TEST] [--backtest] [--faults N] [--watch] [-- ARGS...]\n  \
                 rustd patch  [--manifest-path PATH] [--bin NAME]\n  \
                 rustd sync   [--manifest-path PATH] [--bin NAME]\n  \
                 rustd status [--manifest-path PATH] [--bin NAME]\n  \
                 rustd stop   [--manifest-path PATH] [--bin NAME]\n\n\
                 Low-level daemon invocation:\n  \
                 rustd <Cargo.toml> [--crate <CRATE>] --out <BASE.O> [--patch <PATCH.O>] [--symbols <RLIB>]... [--watch]"
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
    run_args: Vec<String>,
}

struct TargetPaths {
    dir: PathBuf,
    base_o: PathBuf,
    rt_o: PathBuf,
    patch_o: PathBuf,
    ack: PathBuf,
    out_bin: PathBuf,
    rustd_sock: PathBuf,
    rustd_log: PathBuf,
    pid_file: PathBuf,
    link_args_txt: PathBuf,
    link_sh: PathBuf,
    hotpatch_map: PathBuf,
}

impl TargetPaths {
    fn new(workspace_root: &Path, bin_name: &str) -> Self {
        let dir = workspace_root.join("target").join("rustd").join(bin_name);
        let base_o = dir.join("base.o");
        let rt_o = dir.join("base.rt.o");
        let patch_o = dir.join("patch.o");
        let ack = dir.join("patch.ack");
        Self {
            out_bin: dir.join(bin_name),
            rustd_sock: dir.join("rustd.sock"),
            rustd_log: dir.join("rustd.log"),
            pid_file: dir.join("rustd.pid"),
            link_args_txt: dir.join("link_args.txt"),
            link_sh: dir.join("link.sh"),
            hotpatch_map: dir.join("hotpatch_map.json"),
            dir,
            base_o,
            rt_o,
            patch_o,
            ack,
        }
    }
}

fn find_manifest_path(explicit: Option<PathBuf>) -> anyhow::Result<(PathBuf, PathBuf)> {
    if let Some(p) = explicit {
        let abs = fs::canonicalize(&p).with_context(|| format!("finding {}", p.display()))?;
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
        let mut active_dirs = Vec::new();
        for e in entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
            if let Ok(name) = e.file_name().into_string() {
                if e.path().join("rustd.sock").exists() {
                    active_dirs.push(name.clone());
                }
                all_dirs.push(name);
            }
        }
        if active_dirs.len() == 1 {
            return active_dirs.pop();
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
                test_target = Some(it.next().context("--test requires a test target name")?.clone());
            }
            "-p" | "--package" if cfg_test => {
                pkg_name = Some(it.next().context("-p requires a package name")?.clone());
            }
            "--crate" | "-p" | "--package" => {
                let val = it.next().context("--crate requires a crate name")?;
                if cfg_test && pkg_name.is_none() {
                    pkg_name = Some(val.clone());
                } else {
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
            "--workspace"
            | "--all-targets"
            | "--lib"
            | "--bins"
            | "--tests"
            | "--link-only"
            | "--cargo"
            | "--full"
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
                also_run = Some(PathBuf::from(it.next().context("--also-run requires a file path")?));
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
        let inferred: Vec<String> = crate::ci::infer_packages_from_diff(&workspace_root, &diff_files)
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
        match bin_name.or_else(|| infer_default_bin(&workspace_root)) {
            Some(b) => b,
            None => bail!("could not infer binary target; pass `--bin <NAME>`"),
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
        run_args,
    })
}

fn parse_opts(args: &[String]) -> anyhow::Result<CliOpts> {
    parse_opts_with_mode(args, false)
}

fn find_wild() -> anyhow::Result<PathBuf> {
    if let Some(p) = env::var_os("WILD_BIN") {
        return Ok(PathBuf::from(p));
    }
    if let Some(home) = env::var_os("HOME") {
        let cargo_wild = PathBuf::from(home).join(".cargo/bin/wild");
        if cargo_wild.exists() {
            return Ok(cargo_wild);
        }
    }
    Ok(PathBuf::from("wild"))
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
        .filter(|p| p.extension().is_some_and(|ext| ext == "rlib"))
        .collect()
}

fn sysroot_rlibs(workspace_root: &Path, target: Option<&str>) -> anyhow::Result<Vec<PathBuf>> {
    let out = Command::new("rustc")
        .current_dir(workspace_root)
        .args(["--print", "sysroot"])
        .output()
        .context("running `rustc --print sysroot`")?;
    let sysroot = String::from_utf8(out.stdout)?.trim().to_owned();
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
    Ok(collect_rlibs_in(&PathBuf::from(format!("{sysroot}/lib/rustlib/{triple}/lib"))))
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
    let feat_file = paths.dir.join("features.key");
    let expected = features_fingerprint(opts);
    if fs::read_to_string(&feat_file).ok().as_deref() != Some(expected.as_str()) {
        stop_target_daemon(paths);
        let _ = fs::remove_file(&paths.link_args_txt);
        let _ = fs::remove_file(paths.dir.join("clippy.sh"));
        let _ = fs::remove_file(paths.out_bin.with_extension("base"));
        let _ = fs::create_dir_all(&paths.dir);
        let _ = fs::write(&feat_file, expected);
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
                .rfind(|l| l.contains("-o") && (l.contains("cc") || l.contains("clang")))
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
        let _ = fs::remove_file(paths.out_bin.with_extension("base"));
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
        cmd.args(["--", "-C", "save-temps"]);
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
            "cargo test --no-run did not emit a linker command for `{pkg}`"
        );
        fs::write(&paths.link_args_txt, &combined)?;
        return Ok(combined);
    }
    eprintln!("Capturing linker arguments via `cargo build --bin {}`...", opts.bin_name);
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
    cmd.current_dir(&opts.workspace_root)
        .args(["rustc", "--bin", &opts.bin_name])
        .env("RUSTC_LOG", "rustc_codegen_ssa::back::link=info");
    apply_feature_args(&mut cmd, opts);
    if let Some(t) = &opts.target {
        cmd.args(["--target", t]);
    }
    cmd.args(["--", "-C", "save-temps"]);
    let out = cmd.output().context("running cargo rustc to capture link args")?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    ensure!(
        combined.lines().any(|l| l.contains("-o") && (l.contains("cc") || l.contains("clang") || l.contains("rust-lld"))),
        "cargo build did not emit a linker command for `{}`",
        opts.bin_name
    );
    fs::write(&paths.link_args_txt, &combined)?;
    Ok(combined)
}

fn prepare_link_script_and_symbols(
    opts: &CliOpts,
    paths: &TargetPaths,
) -> anyhow::Result<Vec<PathBuf>> {
    fs::create_dir_all(&paths.dir)?;
    let wild_bin = find_wild()?;
    let wild_str = wild_bin.display().to_string();
    let link_text = ensure_link_args(opts, paths)?;
    let is_wasm = opts.target.as_deref().is_some_and(|t| t.starts_with("wasm32"));

    let mut rlibs = Vec::new();
    if is_wasm {
        let line = link_text
            .lines()
            .find(|l| l.contains("rust-lld"))
            .context("no rust-lld invocation found in link_args.txt")?;
        let words = shell_words(line);
        let idx = words.iter().position(|w| w == "rust-lld").context("no rust-lld")?;
        let link_args = &words[idx..];
        let first_obj = link_args
            .iter()
            .position(|a| a.ends_with(".o") || a.ends_with(".rlib"))
            .context("no .o/.rlib in wasm link args")?;
        let mut wild_cmd = vec![wild_str];
        wild_cmd.extend_from_slice(&link_args[1..first_obj]);
        wild_cmd.push("--allow-multiple-definition".to_owned());
        wild_cmd.push(paths.patch_o.display().to_string());
        wild_cmd.push(paths.base_o.display().to_string());
        wild_cmd.extend_from_slice(&link_args[first_obj..]);
        if let Some(o_pos) = wild_cmd.iter().position(|x| x == "-o") {
            wild_cmd[o_pos + 1] = paths.out_bin.display().to_string();
        }
        let sh = format!(
            "#!/bin/sh\nexport WILD_HOTPATCH_MAP={}\nexec {}\n",
            quote_sh(&paths.hotpatch_map.display().to_string()),
            wild_cmd.iter().map(|s| quote_sh(s)).collect::<Vec<_>>().join(" ")
        );
        fs::write(&paths.link_sh, sh)?;
        fs::set_permissions(&paths.link_sh, fs::Permissions::from_mode(0o755))?;
        for a in link_args {
            if (a.ends_with(".rlib") || a.ends_with(".o")) && Path::new(a).exists() {
                rlibs.push(PathBuf::from(a));
            }
        }
    } else {
        let line = link_text
            .lines()
            .rfind(|l| l.contains("-o") && (l.contains("cc") || l.contains("clang")))
            .context("no cc/clang invocation found in link_args.txt")?;
        let words = shell_words(line);
        let idx = words
            .iter()
            .position(|w| w.ends_with("cc") || w.ends_with("clang"))
            .context("no cc/clang token in link line")?;
        let link_args: Vec<String> = words[idx..]
            .iter()
            .filter(|a| !(a.ends_with("symbols.o") && a.contains("rustc")))
            .cloned()
            .collect();
        let first_obj = link_args
            .iter()
            .position(|a| a.ends_with(".o") || a.ends_with(".rlib"))
            .context("no .o/.rlib in native link args")?;

        let ld_wrapper = paths.dir.join("ld");
        let _ = fs::remove_file(&ld_wrapper);
        let flavor = if cfg!(target_os = "macos") { "-flavor darwin " } else { "" };
        fs::write(
            &ld_wrapper,
            format!("#!/bin/sh\nexec {} {flavor}\"$@\"\n", quote_sh(&wild_str)),
        )?;
        fs::set_permissions(&ld_wrapper, fs::Permissions::from_mode(0o755))?;

        let sys_rlibs = sysroot_rlibs(&opts.workspace_root, opts.target.as_deref())?;
        let mut wild_cc = vec![
            link_args[0].clone(),
            format!("-B{}", paths.dir.display()),
            "-Wl,--allow-multiple-definition".to_owned(),
        ];
        if !cfg!(target_os = "macos") {
            wild_cc.push("-Wl,--export-dynamic".to_owned());
        }
        wild_cc.extend_from_slice(&link_args[1..first_obj]);
        wild_cc.push(paths.rt_o.display().to_string());
        wild_cc.push(paths.patch_o.display().to_string());
        wild_cc.push(paths.base_o.display().to_string());
        wild_cc.extend_from_slice(&link_args[first_obj..]);
        if cfg!(target_os = "macos") {
            for sr in &sys_rlibs {
                if let Some(fname) = sr.file_name().and_then(|n| n.to_str())
                    && (fname.starts_with("libcore-") || fname.starts_with("liballoc-"))
                {
                    wild_cc.push(format!("-Wl,-force_load,{}", sr.display()));
                }
            }
        } else {
            for sr in &sys_rlibs {
                if let Some(fname) = sr.file_name().and_then(|n| n.to_str())
                    && (fname.starts_with("libcore-") || fname.starts_with("liballoc-"))
                {
                    wild_cc.push(format!(
                        "-Wl,--whole-archive,{},--no-whole-archive",
                        sr.display()
                    ));
                }
            }
        }
        wild_cc.push(format!("-Wl,{}", paths.base_o.with_extension("cave.o").display()));
        if let Some(o_pos) = wild_cc.iter().position(|x| x == "-o") {
            wild_cc[o_pos + 1] = paths.out_bin.display().to_string();
        }
        let sh = format!(
            "#!/bin/sh\nexec {}\n",
            wild_cc.iter().map(|s| quote_sh(s)).collect::<Vec<_>>().join(" ")
        );
        fs::write(&paths.link_sh, sh)?;
        fs::set_permissions(&paths.link_sh, fs::Permissions::from_mode(0o755))?;
        for a in &link_args {
            if (a.ends_with(".rlib") || a.ends_with(".o")) && Path::new(a).exists() {
                rlibs.push(PathBuf::from(a));
            }
        }
        let base_bin = paths.out_bin.with_extension("base");
        if let Ok(bm) = fs::metadata(&base_bin).and_then(|m| m.modified())
            && rlibs.iter().any(|r| {
                fs::metadata(r).and_then(|m| m.modified()).is_ok_and(|rm| rm > bm)
            })
        {
            let _ = fs::remove_file(&base_bin);
        }
    }
    rlibs.extend(sysroot_rlibs(&opts.workspace_root, opts.target.as_deref())?);
    Ok(rlibs)
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

fn ensure_daemon(opts: &CliOpts, paths: &TargetPaths) -> anyhow::Result<Option<String>> {
    ensure_features_consistent(opts, paths);
    let bin_crate = opts
        .test_target
        .as_deref()
        .or(opts.pkg_name.as_deref())
        .unwrap_or(&opts.bin_name)
        .replace('-', "_");
    if paths.rustd_sock.exists() {
        let is_symlink = fs::symlink_metadata(&paths.rustd_sock)
            .is_ok_and(|m| m.file_type().is_symlink());
        let req = if is_symlink {
            format!(
                "switch\t{bin_crate}\t{}\t{}\t{}",
                paths.base_o.display(),
                paths.patch_o.display(),
                paths.link_sh.display()
            )
        } else {
            "patch".to_owned()
        };
        if let Ok(reply) = send_unix_line(&paths.rustd_sock, &req) {
            if !reply.starts_with("error: unknown request") {
                return Ok(Some(reply));
            }
        }
        stop_target_daemon(paths);
    }
    let _ = fs::remove_file(&paths.rustd_sock);
    let _ = fs::remove_file(&paths.ack);
    let rlibs = prepare_link_script_and_symbols(opts, paths)?;

    // Check if another daemon is already running in this workspace with the same feature set
    let rustd_root = opts.workspace_root.join("target").join("rustd");
    let expected_feat = features_fingerprint(opts);
    if let Ok(entries) = fs::read_dir(&rustd_root) {
        for e in entries.flatten() {
            let dir = e.path();
            if dir == paths.dir || !dir.is_dir() {
                continue;
            }
            let other_sock = dir.join("rustd.sock");
            let other_feat = fs::read_to_string(dir.join("features.key")).unwrap_or_default();
            if other_sock.exists() && other_feat == expected_feat {
                if !paths.out_bin.exists() {
                    let base_src = rustd_root
                        .iter()
                        .next()
                        .and_then(|_| dir.join("base.o").exists().then(|| dir.join("base.o")));
                    if let Some(bs) = base_src {
                        let _ = fs::copy(&bs, &paths.base_o);
                        let _ = fs::copy(bs.with_extension("rt.o"), &paths.rt_o);
                        let _ = fs::copy(
                            bs.with_extension("cave.o"),
                            paths.base_o.with_extension("cave.o"),
                        );
                        let _ = fs::copy(&bs, &paths.patch_o);
                        let _ = Command::new("sh").arg("-c").arg(&paths.link_sh).status();
                    }
                }
                let switch_req = format!(
                    "switch\t{bin_crate}\t{}\t{}\t{}",
                    paths.base_o.display(),
                    paths.patch_o.display(),
                    paths.link_sh.display()
                );
                if let Ok(reply) = send_unix_line(&other_sock, &switch_req) {
                    let _ = std::os::unix::fs::symlink(&other_sock, &paths.rustd_sock);
                    return Ok(Some(reply));
                }
            }
        }
    }

    let exe = env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.current_dir(&opts.workspace_root).arg(&opts.manifest_path);
    if let Some(t) = &opts.target {
        cmd.args(["--target", t]);
    }
    for c in &opts.crates {
        cmd.args(["--crate", c]);
    }
    if opts.cfg_test {
        cmd.arg("--cfg-test");
    }
    if !opts.features.is_empty() {
        cmd.args(["--features", &opts.features.join(",")]);
    }
    if let Some(pkg) = &opts.pkg_name {
        cmd.env("RUSTD_PKG", pkg);
    }
    cmd.arg("--out")
        .arg(&paths.base_o)
        .arg("--patch")
        .arg(&paths.patch_o)
        .arg("--watch")
        .arg("--socket")
        .arg(&paths.rustd_sock)
        .arg("--link-cmd")
        .arg(&paths.link_sh)
        .env("RUSTD_BIN_CRATE", &bin_crate)
        .env("RUSTD_STATS", "1");
    if opts.target.as_deref().is_some_and(|t| t.starts_with("wasm32")) {
        cmd.env("RUSTD_WASM_TRAMPOLINES", "1");
    } else if !opts.crates.is_empty() {
        cmd.env("RUSTD_NATIVE_TRAMPOLINES", "1");
    }
    for r in &rlibs {
        cmd.arg("--symbols").arg(r);
    }

    let scope_desc = if opts.crates.is_empty() {
        "all workspace crates".to_owned()
    } else {
        format!("crates: {}", opts.crates.join(", "))
    };
    eprintln!(
        "[rustd] starting hot-patch daemon for `{}` ({scope_desc})...",
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
    let reply = send_unix_line(&paths.rustd_sock, "patch").ok();
    Ok(reply)
}

fn check_compile_error(opts: &CliOpts, paths: &TargetPaths, reply: Option<&str>) -> bool {
    let Some(r) = reply else { return false };
    if !r.starts_with("error:") {
        return false;
    }
    let check_txt = paths.base_o.with_extension("check.txt");
    if print_check_file(opts, &check_txt, None) == 0 {
        eprintln!(
            "\x1b[1;31merror\x1b[0m\x1b[1m: {}\x1b[0m",
            r.trim_start_matches("error:").trim()
        );
    }
    true
}

fn print_check_file(opts: &CliOpts, check_txt: &Path, fallback_elapsed: Option<Duration>) -> usize {
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let mut us = 0u64;
    if let Ok(content) = fs::read_to_string(check_txt) {
        let (header, body) = content.split_once('\n').unwrap_or((&content, ""));
        for word in header.split_whitespace() {
            if let Some(val) = word.strip_prefix("errors=") {
                errors = val.parse().unwrap_or(0);
            } else if let Some(val) = word.strip_prefix("warnings=") {
                warnings = val.parse().unwrap_or(0);
            } else if let Some(val) = word.strip_prefix("us=") {
                us = val.parse().unwrap_or(0);
            }
        }
        if !body.is_empty() {
            eprint!("{body}");
        }
    }
    let elapsed = fallback_elapsed.unwrap_or_else(|| Duration::from_micros(us));
    let raw_label = opts.pkg_name.as_deref().unwrap_or(&opts.bin_name);
    let target_label = raw_label.strip_prefix("test-").unwrap_or(raw_label);
    if warnings > 0 {
        let w_plural = if warnings == 1 { "" } else { "s" };
        eprintln!(
            "\x1b[1;33mwarning\x1b[0m\x1b[1m: `{target_label}` generated {warnings} warning{w_plural}\x1b[0m"
        );
    }
    if errors > 0 {
        let plural = if errors == 1 { "" } else { "s" };
        eprintln!(
            "\x1b[1;31merror\x1b[0m\x1b[1m: could not compile `{target_label}` due to {errors} previous error{plural}\x1b[0m"
        );
    } else {
        eprintln!(
            "    \x1b[1;32mFinished\x1b[0m `dev` profile [unoptimized + debuginfo] target(s) in {:.2}s ({:.1?})",
            elapsed.as_secs_f64(),
            elapsed
        );
    }
    errors
}

fn run_one_check(opts: &CliOpts, paths: &TargetPaths, req_cmd: &str) -> anyhow::Result<usize> {
    let t0 = Instant::now();
    let _reply = send_unix_line(&paths.rustd_sock, req_cmd)?;
    let elapsed = t0.elapsed();
    let check_txt = paths.base_o.with_extension("check.txt");
    Ok(print_check_file(opts, &check_txt, Some(elapsed)))
}

fn cmd_check(args: &[String], req_cmd: &str) -> anyhow::Result<()> {
    let has_test_flag = args
        .iter()
        .any(|a| a == "-p" || a == "--package" || a == "--test" || a == "--tests" || a == "--since");
    let mut opts = parse_opts_with_mode(args, has_test_flag)?;
    if opts.bin_name.starts_with("test-") {
        opts.cfg_test = true;
    }
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    if let Some(sha) = &opts.since {
        let diff_files = crate::ci::analyze_git_diff_since(&opts.workspace_root, sha)?;
        fs::create_dir_all(&paths.dir)?;
        let baseline_cache = paths.base_o.with_extension("baseline.cache");
        crate::ci::write_since_baseline_cache(
            &baseline_cache,
            &format!("SINCE:{sha}"),
            &diff_files,
        )?;
    }
    ensure_daemon(&opts, &paths)?;
    let errors = run_one_check(&opts, &paths, req_cmd)?;
    if !opts.watch {
        if errors > 0 {
            std::process::exit(101);
        }
        return Ok(());
    }
    let max_runs = env::var("RUSTD_WATCH_MAX_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    let mut runs = 1usize;
    if max_runs.is_some_and(|m| runs >= m) {
        return Ok(());
    }
    let check_txt = paths.base_o.with_extension("check.txt");
    let mut last_mtime = fs::metadata(&check_txt).and_then(|m| m.modified()).ok();
    eprintln!("[rustd] watching for changes... (Ctrl-C to exit)");
    loop {
        thread::sleep(Duration::from_millis(20));
        let cur_mtime = fs::metadata(&check_txt).and_then(|m| m.modified()).ok();
        if cur_mtime.is_some() && cur_mtime != last_mtime {
            last_mtime = cur_mtime;
            print_check_file(&opts, &check_txt, None);
            runs += 1;
            if max_runs.is_some_and(|m| runs >= m) {
                return Ok(());
            }
        }
    }
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
    let mut skip_next = false;
    let mut saw_incr = false;
    let incr_dir = paths.dir.join(if with_tests {
        "clippy-incr-tests"
    } else {
        "clippy-incr"
    });
    for w in words {
        if skip_next {
            skip_next = false;
            continue;
        }
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
    let _ = Command::new(&clippy_sh)
        .current_dir(&opts.workspace_root)
        .output();
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
    let fast = args.iter().any(|a| a == "--fast");
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
    if fast {
        return cmd_check(args, "clippy");
    }
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    let clippy_sh = ensure_clippy_script(&opts, &paths, with_tests)?;
    let clippy_cache = paths.dir.join(if with_tests {
        "clippy_tests.cache"
    } else {
        "clippy.cache"
    });
    let run_once = || -> anyhow::Result<bool> {
        let t0 = Instant::now();
        let extra_key = format!("{}:{:?}", clippy_sh.display(), opts.run_args);
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
            .args(&opts.run_args)
            .output()
            .context("running cached clippy-driver")?;
        let elapsed = t0.elapsed();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let ok = out.status.success();
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
    if !opts.watch {
        if !ok {
            std::process::exit(101);
        }
        return Ok(());
    }
    eprintln!("[rustd] watching for changes... (Ctrl-C to exit)");
    let pkg = opts.pkg_name.as_deref().unwrap_or(&opts.bin_name);
    let mut last_max_mtime: Option< std::time::SystemTime> = None;
    loop {
        thread::sleep(Duration::from_millis(50));
        let mut cur_max: Option<std::time::SystemTime> = None;
        let mut stack = vec![opts.workspace_root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if !p.ends_with("target") && !p.ends_with(".git") {
                        stack.push(p);
                    }
                } else if p.extension().is_some_and(|ext| ext == "rs")
                    && let Ok(m) = p.metadata().and_then(|m| m.modified())
                {
                    cur_max = Some(cur_max.map_or(m, |prev| prev.max(m)));
                }
            }
        }
        if last_max_mtime.is_none() {
            last_max_mtime = cur_max;
            continue;
        }
        if cur_max != last_max_mtime {
            last_max_mtime = cur_max;
            eprintln!("    \x1b[1;32mChecking\x1b[0m {pkg}...");
            let _ = run_once()?;
        }
    }
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
            if p.ends_with(".rs") && !rs_files.iter().any(|x| x == p) && workspace_root.join(p).exists() {
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

fn cmd_rebase(args: &[String]) -> anyhow::Result<()> {
    let has_test_flag = args
        .iter()
        .any(|a| a == "-p" || a == "--package" || a == "--test" || a == "--tests");
    let force_cargo = args.iter().any(|a| a == "--cargo");
    let link_only = args.iter().any(|a| a == "--link-only");
    let mut opts = parse_opts_with_mode(args, has_test_flag)?;
    if opts.bin_name.starts_with("test-") {
        opts.cfg_test = true;
    }
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    if !force_cargo {
        let _ = ensure_daemon(&opts, &paths)?;
        let t0 = Instant::now();
        if let Ok(reply) = send_unix_line(&paths.rustd_sock, "rebase\n")
            && reply.starts_with("ok")
        {
            eprintln!(
                "[rustd] rebased `{}` via wild full link in {:.2?} ({reply})",
                opts.bin_name,
                t0.elapsed()
            );
            return Ok(());
        } else if link_only {
            anyhow::bail!("--link-only rebase failed; run `rustd rebase --cargo` for full rebuild");
        }
    }
    eprintln!(
        "[rustd] rebasing `{}` with full LLVM/cargo optimization...",
        opts.bin_name
    );
    stop_target_daemon(&paths);
    let _ = fs::remove_file(&paths.link_args_txt);
    let _ = fs::remove_file(paths.out_bin.with_extension("base"));
    let _ = fs::remove_file(paths.base_o.with_extension("baseline.cache"));
    let _ = fs::remove_file(&paths.patch_o);
    ensure_daemon(&opts, &paths)?;
    eprintln!(
        "[rustd] rebase complete for `{}` (dirty patches folded into LLVM base binary)",
        opts.bin_name
    );
    Ok(())
}

fn cmd_build(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_opts(args)?;
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    if opts.watch || paths.rustd_sock.exists() {
        let reply = ensure_daemon(&opts, &paths)?;
        if !opts.watch && check_compile_error(&opts, &paths, reply.as_deref()) {
            std::process::exit(101);
        }
        println!("{}", paths.out_bin.display());
        return Ok(());
    }
    let rlibs = prepare_link_script_and_symbols(&opts, &paths)?;
    let exe = env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.current_dir(&opts.workspace_root).arg(&opts.manifest_path);
    if let Some(t) = &opts.target {
        cmd.args(["--target", t]);
    }
    for c in &opts.crates {
        cmd.args(["--crate", c]);
    }
    cmd.arg("--out")
        .arg(&paths.base_o)
        .arg("--patch")
        .arg(&paths.patch_o)
        .arg("--link-cmd")
        .arg(&paths.link_sh);
    for r in &rlibs {
        cmd.arg("--symbols").arg(r);
    }
    let status = cmd.status()?;
    ensure!(status.success(), "rustd build failed ({status})");
    println!("{}", paths.out_bin.display());
    Ok(())
}

fn cmd_run(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_opts(args)?;
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    let reply = ensure_daemon(&opts, &paths)?;
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
        let _ = fs::copy(&lib_paths.rt_o, &t_paths.rt_o);
        let _ = fs::copy(
            lib_paths.base_o.with_extension("cave.o"),
            t_paths.base_o.with_extension("cave.o"),
        );
        let _ = fs::copy(&lib_paths.patch_o, &t_paths.patch_o);
        if let Ok(sh_txt) = fs::read_to_string(&t_paths.link_sh) {
            let patched_sh = sh_txt
                .replace(
                    &t_paths.rt_o.display().to_string(),
                    &lib_paths.rt_o.display().to_string(),
                )
                .replace(
                    &t_paths.patch_o.display().to_string(),
                    &lib_paths.patch_o.display().to_string(),
                );
            let _ = fs::write(&t_paths.link_sh, patched_sh);
        }
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
    ensure_daemon(&opts, &paths)?;
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

    if let Some(sha) = &opts.since {
        let diff_files = crate::ci::analyze_git_diff_since(&opts.workspace_root, sha)?;
        fs::create_dir_all(&paths.dir)?;
        let baseline_cache = paths.base_o.with_extension("baseline.cache");
        crate::ci::write_since_baseline_cache(
            &baseline_cache,
            &format!("SINCE:{sha}"),
            &diff_files,
        )?;
        let since_sha_file = paths.base_o.with_extension("since.sha");
        let _ = fs::write(&since_sha_file, format!("{sha}\n"));
    }

    let reply = ensure_daemon(&opts, &paths)?;
    let cleanup_oneshot = || {};

    if opts.backtest {
        let bt_reply =
            send_unix_line(&paths.rustd_sock, &format!("backtest {}", opts.faults))?;
        ensure!(!bt_reply.starts_with("error:"), "rustd backtest: {bt_reply}");
        let bt_txt = paths.base_o.with_extension("backtest.txt");
        if let Ok(report) = fs::read_to_string(&bt_txt) {
            print!("{report}");
        }
        cleanup_oneshot();
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
        cleanup_oneshot();
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
        cleanup_oneshot();
        return Ok(());
    }

    let is_ci_runner = opts.since.is_some()
        || opts.junit.is_some()
        || opts.json
        || opts.also_run.is_some()
        || opts.retries.is_some();

    if is_ci_runner && !opts.watch {
        if check_compile_error(&opts, &paths, reply.as_deref()) {
            cleanup_oneshot();
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
        cleanup_oneshot();
        if zero_matched_count > 0 {
            std::process::exit(2);
        }
        if failed > 0 {
            std::process::exit(101);
        }
        return Ok(());
    }

    if !opts.watch {
        if check_compile_error(&opts, &paths, reply.as_deref()) {
            std::process::exit(101);
        }
        let test_args = resolve_test_args(&opts, &paths);
        let mut cmd = Command::new(&paths.out_bin);
        cmd.args(&test_args);
        let err = cmd.exec();
        return Err(err.into());
    }
    let max_runs = env::var("RUSTD_WATCH_MAX_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    let mut runs = 0usize;
    let check_txt = paths.base_o.with_extension("check.txt");
    if !check_compile_error(&opts, &paths, reply.as_deref()) {
        let t0 = Instant::now();
        let initial_args = resolve_test_args(&opts, &paths);
        let status = Command::new(&paths.out_bin).args(&initial_args).status()?;
        eprintln!("[rustd] initial test run finished in {:.1?} ({status})", t0.elapsed());
    }
    runs += 1;
    if max_runs.is_some_and(|m| runs >= m) {
        return Ok(());
    }
    let mut last_patch_mtime = fs::metadata(&paths.patch_o).and_then(|m| m.modified()).ok();
    let mut last_check_mtime = fs::metadata(&check_txt).and_then(|m| m.modified()).ok();
    eprintln!("[rustd] watching for changes... (Ctrl-C to exit)");
    loop {
        thread::sleep(Duration::from_millis(20));
        let cur_patch_mtime = fs::metadata(&paths.patch_o).and_then(|m| m.modified()).ok();
        let cur_check_mtime = fs::metadata(&check_txt).and_then(|m| m.modified()).ok();
        if (cur_patch_mtime.is_some() && cur_patch_mtime != last_patch_mtime)
            || (cur_check_mtime.is_some() && cur_check_mtime != last_check_mtime)
        {
            last_patch_mtime = cur_patch_mtime;
            last_check_mtime = cur_check_mtime;
            let has_compile_errors = fs::read_to_string(&check_txt).is_ok_and(|content| {
                content
                    .split_once('\n')
                    .is_some_and(|(h, _)| h.contains("errors=") && !h.starts_with("errors=0 "))
            });
            if has_compile_errors {
                print_check_file(&opts, &check_txt, None);
            } else {
                let t0 = Instant::now();
                let next_args = resolve_test_args(&opts, &paths);
                let status = Command::new(&paths.out_bin).args(&next_args).status()?;
                eprintln!("[rustd] re-ran tests in {:.1?} ({status})", t0.elapsed());
            }
            runs += 1;
            if max_runs.is_some_and(|m| runs >= m) {
                return Ok(());
            }
        }
    }
}

fn cmd_request(req: &str, args: &[String]) -> anyhow::Result<()> {
    let opts = parse_opts(args)?;
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    let _ = fs::remove_file(&paths.ack);
    let t0 = Instant::now();
    let reply = send_unix_line(&paths.rustd_sock, req).with_context(|| {
        format!(
            "no running rustd daemon at {}; run `rustd build --watch` or `rustd run` first",
            paths.rustd_sock.display()
        )
    })?;
    let compile_ms = t0.elapsed().as_secs_f64() * 1000.0;
    ensure!(!reply.starts_with("error:"), "rustd: {reply}");
    println!("{reply} (client_ms={compile_ms:.2})");
    if req == "patch" && reply.contains("patch=") {
        let deadline = Instant::now() + Duration::from_millis(50);
        while Instant::now() < deadline {
            if let Ok(txt) = fs::read_to_string(&paths.ack) {
                let mut parts = txt.split_whitespace();
                if let (Some(us), Some(cnt)) = (
                    parts.next().and_then(|s| s.parse::<u64>().ok()),
                    parts.next().and_then(|s| s.parse::<usize>().ok()),
                ) {
                    println!(
                        "[rustd] live process hot-swapped {cnt} fn(s) in {:.3} ms",
                        us as f64 / 1000.0
                    );
                    break;
                }
            }
            thread::sleep(Duration::from_micros(500));
        }
    }
    Ok(())
}

fn cmd_status(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_opts(args)?;
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    if !paths.rustd_sock.exists() {
        println!("not running ({})", paths.dir.display());
        return Ok(());
    }
    let pid = fs::read_to_string(&paths.pid_file).unwrap_or_default();
    println!(
        "running: bin={} pid={} socket={} binary={}",
        opts.bin_name,
        pid.trim(),
        paths.rustd_sock.display(),
        paths.out_bin.display()
    );
    Ok(())
}

fn cmd_stop(args: &[String], quiet: bool) -> anyhow::Result<()> {
    let opts = parse_opts(args)?;
    let rustd_dir = opts.workspace_root.join("target").join("rustd");
    let mut stopped = Vec::new();
    if let Ok(entries) = fs::read_dir(&rustd_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && (p.join("rustd.sock").exists() || p.join("rustd.pid").exists()) {
                if let Ok(pid_str) = fs::read_to_string(p.join("rustd.pid"))
                    && let Ok(pid) = pid_str.trim().parse::<u32>()
                    && pid > 0
                {
                    let _ = Command::new("kill").arg("-15").arg(pid.to_string()).status();
                }
                let _ = fs::remove_file(p.join("rustd.sock"));
                let _ = fs::remove_file(p.join("rustd.pid"));
                if let Ok(name) = e.file_name().into_string() {
                    stopped.push(name);
                }
            }
        }
    }
    if stopped.is_empty() {
        let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
        let _ = fs::remove_file(&paths.rustd_sock);
        let _ = fs::remove_file(&paths.pid_file);
        stopped.push(opts.bin_name);
    }
    if !quiet {
        println!("stopped rustd daemon for `{}`", stopped.join("`, `"));
    }
    Ok(())
}
