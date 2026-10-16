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
        "build" | "run" | "test" | "patch" | "sync" | "status" | "stop" | "help" | "--help" | "-h"
    )
}

pub(crate) fn dispatch(sub: &str, args: Vec<String>) -> anyhow::Result<()> {
    match sub {
        "build" => cmd_build(&args),
        "run" => cmd_run(&args),
        "test" => cmd_test(&args),
        "patch" => cmd_request("patch", &args),
        "sync" => cmd_request("sync", &args),
        "status" => cmd_status(&args),
        "stop" => cmd_stop(&args, false),
        _ => {
            eprintln!(
                "Usage:\n  \
                 rustd build [--manifest-path PATH] [--bin NAME] [--crate CRATE]... [--target TRIPLE] [--watch]\n  \
                 rustd run   [--manifest-path PATH] [--bin NAME] [--crate CRATE]... [--target TRIPLE] [-- ARGS...]\n  \
                 rustd test  [FILTER] [-p PACKAGE] [--test NAME] [--bin NAME] [--watch] [-- ARGS...]\n  \
                 rustd patch [--manifest-path PATH] [--bin NAME]\n  \
                 rustd sync  [--manifest-path PATH] [--bin NAME]\n  \
                 rustd status [--manifest-path PATH] [--bin NAME]\n  \
                 rustd stop   [--manifest-path PATH] [--bin NAME]\n\n\
                 Low-level daemon invocation:\n  \
                 rustd <Cargo.toml> [--crate <CRATE>] --out <BASE.O> [--patch <PATCH.O>] [--symbols <RLIB>]... [--watch]"
            );
            Ok(())
        }
    }
}

struct CliOpts {
    manifest_path: PathBuf,
    workspace_root: PathBuf,
    bin_name: String,
    pkg_name: Option<String>,
    test_target: Option<String>,
    crates: Vec<String>,
    target: Option<String>,
    watch: bool,
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
    let mut target = None;
    let mut watch = false;
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
            "--target" => {
                target = Some(it.next().context("--target requires a triple")?.clone());
            }
            "-w" | "--watch" => watch = true,
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
    let bin_name = if cfg_test {
        let pkg = pkg_name
            .clone()
            .or_else(|| bin_name.clone())
            .or_else(|| infer_default_bin(&workspace_root))
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
        target,
        watch,
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

fn ensure_link_args(opts: &CliOpts, paths: &TargetPaths) -> anyhow::Result<String> {
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

fn ensure_daemon(opts: &CliOpts, paths: &TargetPaths) -> anyhow::Result<()> {
    if paths.rustd_sock.exists() && send_unix_line(&paths.rustd_sock, "patch").is_ok() {
        return Ok(());
    }
    let _ = fs::remove_file(&paths.rustd_sock);
    let _ = fs::remove_file(&paths.ack);
    let rlibs = prepare_link_script_and_symbols(opts, paths)?;

    let exe = env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.current_dir(&opts.workspace_root).arg(&opts.manifest_path);
    if let Some(t) = &opts.target {
        cmd.args(["--target", t]);
    }
    for c in &opts.crates {
        cmd.args(["--crate", c]);
    }
    let bin_crate = opts
        .test_target
        .as_deref()
        .or(opts.pkg_name.as_deref())
        .unwrap_or(&opts.bin_name)
        .replace('-', "_");
    if opts.cfg_test {
        cmd.arg("--cfg-test");
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
    let _ = send_unix_line(&paths.rustd_sock, "patch");
    Ok(())
}

fn cmd_build(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_opts(args)?;
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    if opts.watch || paths.rustd_sock.exists() {
        ensure_daemon(&opts, &paths)?;
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
    ensure_daemon(&opts, &paths)?;
    let mut cmd = Command::new(&paths.out_bin);
    cmd.args(&opts.run_args);
    let err = cmd.exec();
    Err(err.into())
}

fn cmd_test(args: &[String]) -> anyhow::Result<()> {
    let opts = parse_opts_with_mode(args, true)?;
    let paths = TargetPaths::new(&opts.workspace_root, &opts.bin_name);
    ensure_daemon(&opts, &paths)?;
    if !opts.watch {
        let mut cmd = Command::new(&paths.out_bin);
        cmd.args(&opts.run_args);
        let err = cmd.exec();
        return Err(err.into());
    }
    let max_runs = env::var("RUSTD_WATCH_MAX_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    let mut runs = 0usize;
    let t0 = Instant::now();
    let status = Command::new(&paths.out_bin).args(&opts.run_args).status()?;
    eprintln!("[rustd] initial test run finished in {:.1?} ({status})", t0.elapsed());
    runs += 1;
    if max_runs.is_some_and(|m| runs >= m) {
        return Ok(());
    }
    let mut last_mtime = fs::metadata(&paths.patch_o).and_then(|m| m.modified()).ok();
    eprintln!("[rustd] watching for changes... (Ctrl-C to exit)");
    loop {
        thread::sleep(Duration::from_millis(20));
        let cur_mtime = fs::metadata(&paths.patch_o).and_then(|m| m.modified()).ok();
        if cur_mtime.is_some() && cur_mtime != last_mtime {
            last_mtime = cur_mtime;
            let t0 = Instant::now();
            let status = Command::new(&paths.out_bin).args(&opts.run_args).status()?;
            eprintln!("[rustd] re-ran tests in {:.1?} ({status})", t0.elapsed());
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
