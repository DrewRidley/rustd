//! `rustd surreal` — pure-Rust subsecond-style hot-patch runner for SurrealDB (Native & Wasm).

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    os::unix::{fs::PermissionsExt, net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, bail, ensure};

const BASE_STATE_DIR: &str = "/private/var/folders/fs/wg97w2sx2k730pr2ptnmqcf80000gp/T/opencode";
const DEFAULT_WASM_WORKTREE: &str = "/Users/drewr/Documents/Projects/surrealdb-wt-wasm-s0";
const DEFAULT_NATIVE_WORKTREE: &str = "/Users/drewr/Documents/Projects/surrealdb-wt-rustd";
const WASM_DEPS_DIR: &str = "/Users/drewr/Documents/Projects/size-target-6mb/codeshape-remarks/wasm32-unknown-unknown/edge/deps";
const WASM_SYSROOT_DIR: &str = "/Users/drewr/.rustup/toolchains/nightly-2026-05-11-aarch64-apple-darwin/lib/rustlib/wasm32-unknown-unknown/lib";
const NATIVE_HTTP_PORT: u16 = 18765;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Native,
    Wasm,
}

impl Mode {
    fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "native" => Ok(Self::Native),
            "wasm" => Ok(Self::Wasm),
            _ => bail!("unknown mode `{s}` (expected `native` or `wasm`)"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Wasm => "wasm",
        }
    }
}

struct Paths {
    dir: PathBuf,
    rustd_sock: PathBuf,
    bun_sock: PathBuf,
    base_o: PathBuf,
    patch_o: PathBuf,
    ack: PathBuf,
    out_bin: PathBuf,
    hotpatch_map: PathBuf,
    rustd_log: PathBuf,
    surreal_log: PathBuf,
    link_sh: PathBuf,
    pids_file: PathBuf,
    worktree_file: PathBuf,
}

impl Paths {
    fn for_mode(mode: Mode) -> Self {
        let dir = PathBuf::from(match mode {
            Mode::Wasm => format!("{BASE_STATE_DIR}/surreal_hotpatch"),
            Mode::Native => format!("{BASE_STATE_DIR}/surreal_hotpatch_native"),
        });
        let patch_o = dir.join(if mode == Mode::Wasm { "patch.wasm.o" } else { "patch.o" });
        let ack = patch_o.with_extension("ack");
        Self {
            rustd_sock: dir.join("rustd.sock"),
            bun_sock: dir.join("bun_worker.sock"),
            base_o: dir.join(if mode == Mode::Wasm { "base.wasm.o" } else { "base.o" }),
            patch_o,
            ack,
            out_bin: dir.join(if mode == Mode::Wasm {
                "surreal_hotpatch.wasm"
            } else {
                "surreal-native"
            }),
            hotpatch_map: dir.join("hotpatch_map.json"),
            rustd_log: dir.join("rustd.log"),
            surreal_log: dir.join("surreal_server.log"),
            link_sh: dir.join("wild_link.sh"),
            pids_file: dir.join("pids.txt"),
            worktree_file: dir.join("worktree.txt"),
            dir,
        }
    }
}

pub(crate) fn run(args: Vec<String>) -> anyhow::Result<()> {
    let sub = args.first().map(String::as_str).unwrap_or("help");
    match sub {
        "start" => cmd_start(&args[1..]),
        "sql" => cmd_sql(&args[1..]),
        "query" => cmd_query(&args[1..]),
        "apply-worktree" => cmd_apply_worktree(&args[1..]),
        "reset" => cmd_reset(&args[1..]),
        "status" => cmd_status(&args[1..]),
        "stop" => cmd_stop(&args[1..], false),
        _ => {
            eprintln!(
                "Usage:\n  \
                 rustd surreal start [--mode native|wasm] [--worktree PATH] [--crates CRATES]\n  \
                 rustd surreal sql [SURREAL_SQL_ARGS...]\n  \
                 rustd surreal query \"<SurrealQL>\" [--mode native|wasm] [--repeat N] [--relink]\n  \
                 rustd surreal apply-worktree <OTHER_WORKTREE> [--mode native|wasm] [--query \"<SQL>\"] [--repeat N]\n  \
                 rustd surreal reset [--mode native|wasm] [--query \"<SQL>\"]\n  \
                 rustd surreal status [--mode native|wasm]\n  \
                 rustd surreal stop [--mode native|wasm]"
            );
            Ok(())
        }
    }
}

fn parse_mode(args: &[String], default: Mode) -> anyhow::Result<Mode> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--mode" {
            let val = it.next().context("--mode requires an argument")?;
            return Mode::parse(val);
        }
    }
    Ok(default)
}

fn send_unix_line(sock: &Path, line: &str) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(sock)
        .with_context(|| format!("connecting to {}", sock.display()))?;
    stream.write_all(format!("{line}\n").as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply.trim().to_owned())
}

fn http_post_sql(sql: &str, ns_header: bool) -> anyhow::Result<String> {
    let mut stream =
        TcpStream::connect_timeout(&([127, 0, 0, 1], NATIVE_HTTP_PORT).into(), Duration::from_secs(5))?;
    let body = sql.as_bytes();
    let ns_headers = if ns_header { "Surreal-NS: test\r\nSurreal-DB: test\r\n" } else { "" };
    let req = format!(
        "POST /sql HTTP/1.1\r\n\
         Host: 127.0.0.1:{NATIVE_HTTP_PORT}\r\n\
         Authorization: Basic cm9vdDpyb290\r\n\
         Accept: application/json\r\n\
         {ns_headers}\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes())?;
    stream.write_all(body)?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    let (_, body_str) = resp.split_once("\r\n\r\n").unwrap_or(("", &resp));
    Ok(body_str.trim().to_owned())
}

fn start_native_server(p: &Paths) -> anyhow::Result<u32> {
    let log = fs::File::create(&p.surreal_log)?;
    let log_err = log.try_clone()?;
    let mut child = Command::new(&p.out_bin)
        .args([
            "start",
            "--log",
            "warn",
            "--user",
            "root",
            "--pass",
            "root",
            "--bind",
            &format!("127.0.0.1:{NATIVE_HTTP_PORT}"),
            "memory",
        ])
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .context("spawning surreal-native server")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            let tail = fs::read_to_string(&p.surreal_log).unwrap_or_default();
            bail!("surreal-native exited early ({status}):\n{tail}");
        }
        if http_post_sql(
            "DEFINE NAMESPACE IF NOT EXISTS test; USE NS test; DEFINE DATABASE IF NOT EXISTS test;",
            false,
        )
        .is_ok()
        {
            return Ok(child.id());
        }
        thread::sleep(Duration::from_millis(40));
    }
    bail!("timed out waiting for surreal-native server")
}

fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = line.chars().peekable();
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

fn native_sysroot_rlibs() -> anyhow::Result<Vec<PathBuf>> {
    let out = Command::new("rustup")
        .args(["run", "1.95-aarch64-apple-darwin", "rustc", "--print", "sysroot"])
        .output()?;
    let sysroot = String::from_utf8(out.stdout)?.trim().to_owned();
    let vv = Command::new("rustup")
        .args(["run", "1.95-aarch64-apple-darwin", "rustc", "-vV"])
        .output()?;
    let vv_str = String::from_utf8(vv.stdout)?;
    let host = vv_str
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .context("missing host in rustc -vV")?
        .trim();
    Ok(collect_rlibs_in(&PathBuf::from(format!("{sysroot}/lib/rustlib/{host}/lib"))))
}

fn cmd_start(args: &[String]) -> anyhow::Result<()> {
    let mode = parse_mode(args, Mode::Native)?;
    let p = Paths::for_mode(mode);
    fs::create_dir_all(&p.dir)?;
    cmd_stop(args, true)?;

    let mut worktree = PathBuf::from(if mode == Mode::Wasm {
        DEFAULT_WASM_WORKTREE
    } else {
        DEFAULT_NATIVE_WORKTREE
    });
    let mut crates = vec!["surrealdb_types".to_owned()];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--worktree" => worktree = fs::canonicalize(it.next().context("--worktree path")?)?,
            "--crates" => {
                crates = it
                    .next()
                    .context("--crates list")?
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    fs::write(&p.worktree_file, worktree.display().to_string())?;
    let wild_bin = "/Users/drewr/.cargo/bin/wild";

    let mut rlibs = Vec::new();
    if mode == Mode::Wasm {
        let text = fs::read_to_string(format!("{BASE_STATE_DIR}/wasm_link_args_nolto.txt"))?;
        let line = text.lines().find(|l| l.contains("rust-lld")).context("no rust-lld line")?;
        let words = shell_words(line);
        let idx = words.iter().position(|w| w == "rust-lld").context("no rust-lld")?;
        let link_args = &words[idx..];
        let first_obj = link_args
            .iter()
            .position(|a| a.ends_with(".o") || a.ends_with(".rlib"))
            .context("no object in wasm link args")?;
        let mut wild_cmd = vec![wild_bin.to_owned()];
        wild_cmd.extend_from_slice(&link_args[1..first_obj]);
        wild_cmd.push("--allow-multiple-definition".to_owned());
        wild_cmd.push(p.patch_o.display().to_string());
        wild_cmd.push(p.base_o.display().to_string());
        wild_cmd.extend_from_slice(&link_args[first_obj..]);
        if let Some(o_pos) = wild_cmd.iter().position(|x| x == "-o") {
            wild_cmd[o_pos + 1] = p.out_bin.display().to_string();
        }
        let sh = format!(
            "#!/bin/sh\nexport WILD_HOTPATCH_MAP={}\nexec {}\n",
            quote_sh(&p.hotpatch_map.display().to_string()),
            wild_cmd.iter().map(|s| quote_sh(s)).collect::<Vec<_>>().join(" ")
        );
        fs::write(&p.link_sh, sh)?;
        fs::set_permissions(&p.link_sh, fs::Permissions::from_mode(0o755))?;
        rlibs.extend(collect_rlibs_in(Path::new(WASM_DEPS_DIR)));
        rlibs.extend(collect_rlibs_in(Path::new(WASM_SYSROOT_DIR)));
    } else {
        let _ = fs::remove_file(&p.ack);
        let text = fs::read_to_string(format!("{BASE_STATE_DIR}/surreal_link_args.txt"))?;
        let line = text
            .lines()
            .find(|l| l.contains("-o") && (l.contains("cc") || l.contains("clang")))
            .context("no cc link line in surreal_link_args.txt")?;
        let words = shell_words(line);
        let idx = words
            .iter()
            .position(|w| w.ends_with("cc") || w.ends_with("clang"))
            .context("no cc in link line")?;
        let link_args: Vec<String> = words[idx..]
            .iter()
            .filter(|a| !(a.ends_with("symbols.o") && a.contains("rustc")))
            .cloned()
            .collect();
        let first_obj = link_args
            .iter()
            .position(|a| a.ends_with(".o") || a.ends_with(".rlib"))
            .context("no .o/.rlib in native link args")?;

        let ld_wrapper = p.dir.join("ld");
        let _ = fs::remove_file(&ld_wrapper);
        let flavor = if cfg!(target_os = "macos") { "-flavor darwin " } else { "" };
        fs::write(
            &ld_wrapper,
            format!("#!/bin/sh\nexec {} {flavor}\"$@\"\n", quote_sh(wild_bin)),
        )?;
        fs::set_permissions(&ld_wrapper, fs::Permissions::from_mode(0o755))?;

        let mut wild_cc = vec![
            link_args[0].clone(),
            format!("-B{}", p.dir.display()),
            "-Wl,--allow-multiple-definition".to_owned(),
        ];
        wild_cc.extend_from_slice(&link_args[1..first_obj]);
        wild_cc.push(p.base_o.with_extension("rt.o").display().to_string());
        wild_cc.push(p.patch_o.display().to_string());
        wild_cc.push(p.base_o.display().to_string());
        wild_cc.extend_from_slice(&link_args[first_obj..]);
        if let Some(o_pos) = wild_cc.iter().position(|x| x == "-o") {
            wild_cc[o_pos + 1] = p.out_bin.display().to_string();
        }
        let sh = format!(
            "#!/bin/sh\nexec {}\n",
            wild_cc.iter().map(|s| quote_sh(s)).collect::<Vec<_>>().join(" ")
        );
        fs::write(&p.link_sh, sh)?;
        fs::set_permissions(&p.link_sh, fs::Permissions::from_mode(0o755))?;

        for a in &link_args {
            if (a.ends_with(".rlib") || a.ends_with(".o")) && Path::new(a).exists() {
                rlibs.push(PathBuf::from(a));
            }
        }
        rlibs.extend(native_sysroot_rlibs()?);
    }

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.arg(worktree.join("Cargo.toml"));
    if mode == Mode::Wasm {
        cmd.args(["--target", "wasm32-unknown-unknown"]);
    }
    for c in &crates {
        cmd.args(["--crate", c]);
    }
    cmd.arg("--out")
        .arg(&p.base_o)
        .arg("--patch")
        .arg(&p.patch_o)
        .arg("--watch")
        .arg("--socket")
        .arg(&p.rustd_sock)
        .arg("--link-cmd")
        .arg(&p.link_sh);
    for r in &rlibs {
        cmd.arg("--symbols").arg(r);
    }
    cmd.env(
        "RUSTUP_TOOLCHAIN",
        if mode == Mode::Wasm {
            "nightly-2026-05-11"
        } else {
            "1.95-aarch64-apple-darwin"
        },
    )
    .env("RUSTD_STATS", "1");
    if mode == Mode::Wasm {
        cmd.env("RUSTD_WASM_TRAMPOLINES", "1");
    } else {
        cmd.env("RUSTD_NATIVE_TRAMPOLINES", "1");
    }

    println!(
        "Starting rustd ({}) daemon on {} (crates: {})...",
        mode.as_str(),
        worktree.display(),
        crates.join(", ")
    );
    let t0 = Instant::now();
    let log = fs::File::create(&p.rustd_log)?;
    let log_err = log.try_clone()?;
    let mut rustd_child = cmd
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .context("spawning rustd daemon")?;

    while !p.rustd_sock.exists() {
        if let Some(status) = rustd_child.try_wait()? {
            let tail = fs::read_to_string(&p.rustd_log).unwrap_or_default();
            bail!("rustd exited early ({status}):\n{tail}");
        }
        thread::sleep(Duration::from_millis(100));
    }

    let mut helper_pid = 0u32;
    if mode == Mode::Native {
        helper_pid = start_native_server(&p)?;
    }
    fs::write(&p.pids_file, format!("{} {}\n", rustd_child.id(), helper_pid))?;
    println!(
        "Ready in {:.1?}! (rustd pid={}, surreal-native pid={}, binary={})",
        t0.elapsed(),
        rustd_child.id(),
        helper_pid,
        p.out_bin.display()
    );
    Ok(())
}

fn cmd_sql(args: &[String]) -> anyhow::Result<()> {
    let p = Paths::for_mode(Mode::Native);
    ensure!(
        p.out_bin.exists(),
        "Hot-patchable surreal binary not found at {}; run `rustd surreal start` first",
        p.out_bin.display()
    );
    let mut cmd = Command::new(&p.out_bin);
    cmd.arg("sql");
    if args.is_empty() {
        cmd.args(["--endpoint", "memory", "--ns", "test", "--db", "test"]);
    } else {
        cmd.args(args);
    }
    let err = cmd.exec();
    Err(err.into())
}

fn sync_and_query(mode: Mode, sql: &str, repeat: usize, relink: bool) -> anyhow::Result<()> {
    let p = Paths::for_mode(mode);
    let _ = fs::remove_file(&p.ack);
    let t0 = Instant::now();
    let req = if relink { "sync" } else { "patch" };
    let reply = send_unix_line(&p.rustd_sock, req)?;
    let rustd_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if reply.starts_with("error:") {
        println!("rustd refused edit ({rustd_ms:.2} ms): {reply}");
        return Ok(());
    }
    println!("rustd ({req}): {rustd_ms:.2} ms -> {reply}");

    if mode == Mode::Native {
        if reply.contains("patch=") && !relink {
            let t_swap = Instant::now();
            let deadline = t_swap + Duration::from_millis(500);
            let mut swap_us = None;
            let mut patched_fns = 0usize;
            while Instant::now() < deadline {
                if let Ok(txt) = fs::read_to_string(&p.ack) {
                    let mut parts = txt.split_whitespace();
                    if let (Some(us), Some(cnt)) = (
                        parts.next().and_then(|s| s.parse::<u64>().ok()),
                        parts.next().and_then(|s| s.parse::<usize>().ok()),
                    ) {
                        swap_us = Some(us);
                        patched_fns = cnt;
                        break;
                    }
                }
                thread::sleep(Duration::from_micros(500));
            }
            let swap_ms = swap_us.map_or_else(
                || t_swap.elapsed().as_secs_f64() * 1000.0,
                |us| us as f64 / 1000.0,
            );
            println!(
                "in-process rust hot-swap: {swap_ms:.3} ms ({patched_fns} fn(s) updated in-place, \
                 total edit-to-live: {:.2} ms)",
                rustd_ms + swap_ms
            );
        }
        if !sql.is_empty() {
            let mut times = Vec::with_capacity(repeat);
            let mut last_out = String::new();
            for _ in 0..repeat {
                let qt0 = Instant::now();
                match http_post_sql(sql, true) {
                    Ok(out) => {
                        times.push(qt0.elapsed().as_secs_f64() * 1000.0);
                        last_out = out;
                    }
                    Err(e) => {
                        thread::sleep(Duration::from_millis(50));
                        let tail = fs::read_to_string(&p.surreal_log).unwrap_or_default();
                        println!("surreal-native trapped/exited ({e}):\n{}", tail.trim());
                        let new_pid = start_native_server(&p)?;
                        if let Ok(pids) = fs::read_to_string(&p.pids_file) {
                            let rpid = pids.split_whitespace().next().unwrap_or("0");
                            let _ = fs::write(&p.pids_file, format!("{rpid} {new_pid}\n"));
                        }
                        return Ok(());
                    }
                }
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let min = times[0];
            let med = times[times.len() / 2];
            let mean = times.iter().sum::<f64>() / (times.len() as f64);
            println!("query ({repeat}x): min={min:.3} ms, median={med:.3} ms, mean={mean:.3} ms");
            println!("output: {last_out}");
        }
    } else {
        let did_patch = reply.contains("patch=");
        let did_full = reply.contains("object=");
        if did_full && !relink {
            Command::new(&p.link_sh).status()?;
        }
        let bun_req = format!(
            "{{\"hotpatch\":{},\"reload_full\":{},\"sql\":{:?},\"repeat\":{repeat}}}",
            did_patch && !relink,
            relink || did_full,
            sql
        );
        let bun_reply = send_unix_line(&p.bun_sock, &bun_req)?;
        println!("wasm worker: {bun_reply}");
    }
    Ok(())
}

fn cmd_query(args: &[String]) -> anyhow::Result<()> {
    let mode = parse_mode(args, Mode::Native)?;
    let mut repeat = 1usize;
    let mut relink = false;
    let mut sql = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => {
                it.next();
            }
            "--repeat" => {
                repeat = it.next().context("--repeat N")?.parse()?;
            }
            "--relink" => relink = true,
            s if !s.starts_with("--") && sql.is_empty() => sql = s.to_owned(),
            _ => {}
        }
    }
    sync_and_query(mode, &sql, repeat, relink)
}

fn cmd_apply_worktree(args: &[String]) -> anyhow::Result<()> {
    let mode = parse_mode(args, Mode::Native)?;
    let p = Paths::for_mode(mode);
    let base_wt = PathBuf::from(fs::read_to_string(&p.worktree_file)?.trim());
    let mut other_wt = None;
    let mut query = String::new();
    let mut repeat = 1usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => {
                it.next();
            }
            "--query" => query = it.next().context("--query SQL")?.clone(),
            "--repeat" => repeat = it.next().context("--repeat N")?.parse()?,
            s if !s.starts_with("--") && other_wt.is_none() => {
                other_wt = Some(fs::canonicalize(s)?);
            }
            _ => {}
        }
    }
    let other_wt = other_wt.context("missing <OTHER_WORKTREE> path")?;
    let diff = Command::new("git")
        .arg("-C")
        .arg(&other_wt)
        .args(["diff", "--name-only", "HEAD"])
        .output()?;
    let mut changed: Vec<String> =
        String::from_utf8(diff.stdout)?.lines().map(str::to_owned).collect();

    let base_head = String::from_utf8(
        Command::new("git").arg("-C").arg(&base_wt).args(["rev-parse", "HEAD"]).output()?.stdout,
    )?
    .trim()
    .to_owned();
    let other_head = String::from_utf8(
        Command::new("git").arg("-C").arg(&other_wt).args(["rev-parse", "HEAD"]).output()?.stdout,
    )?
    .trim()
    .to_owned();
    if base_head != other_head {
        let bdiff = Command::new("git")
            .arg("-C")
            .arg(&other_wt)
            .args(["diff", "--name-only", &base_head, &other_head, "--", "surrealdb"])
            .output()?;
        changed.extend(String::from_utf8(bdiff.stdout)?.lines().map(str::to_owned));
    }
    changed.sort();
    changed.dedup();

    let mut copied = Vec::new();
    for rel in changed.into_iter().filter(|f| f.ends_with(".rs")) {
        let src = other_wt.join(&rel);
        let dst = base_wt.join(&rel);
        if src.exists() && dst.exists() && fs::read(&src)? != fs::read(&dst)? {
            fs::copy(&src, &dst)?;
            copied.push(rel);
        }
    }
    println!("Overlaid {} changed .rs file(s) from {}: {:?}", copied.len(), other_wt.display(), copied);
    sync_and_query(mode, &query, repeat, false)
}

fn cmd_reset(args: &[String]) -> anyhow::Result<()> {
    let mode = parse_mode(args, Mode::Native)?;
    let p = Paths::for_mode(mode);
    let mut query = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--query" {
            query = it.next().context("--query SQL")?.clone();
        }
    }
    if p.worktree_file.exists() {
        let base_wt = PathBuf::from(fs::read_to_string(&p.worktree_file)?.trim());
        Command::new("git").arg("-C").arg(&base_wt).args(["checkout", "--", "."]).status()?;
        println!("Reset {} to clean HEAD.", base_wt.display());
    }
    if p.rustd_sock.exists() {
        sync_and_query(mode, &query, 1, false)?;
    }
    Ok(())
}

fn cmd_status(args: &[String]) -> anyhow::Result<()> {
    let mode = parse_mode(args, Mode::Native)?;
    let p = Paths::for_mode(mode);
    if !p.pids_file.exists() {
        println!("Not running ({}).", mode.as_str());
        return Ok(());
    }
    let pids = fs::read_to_string(&p.pids_file)?;
    let wt = fs::read_to_string(&p.worktree_file).unwrap_or_default();
    println!(
        "mode={} worktree={} pids={} binary={}",
        mode.as_str(),
        wt.trim(),
        pids.trim(),
        p.out_bin.display()
    );
    Ok(())
}

fn cmd_stop(args: &[String], quiet: bool) -> anyhow::Result<()> {
    let mode = parse_mode(args, Mode::Native)?;
    let p = Paths::for_mode(mode);
    if let Ok(pids) = fs::read_to_string(&p.pids_file) {
        for tok in pids.split_whitespace() {
            if let Ok(pid) = tok.parse::<u32>()
                && pid > 0
            {
                let _ = Command::new("kill").arg("-15").arg(pid.to_string()).status();
            }
        }
    }
    let _ = fs::remove_file(&p.rustd_sock);
    if !quiet {
        println!("Stopped rustd surreal ({}) daemons.", mode.as_str());
    }
    Ok(())
}
