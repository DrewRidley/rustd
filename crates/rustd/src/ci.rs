//! CI test selection and nextest-style isolated runner for `rustd test --since <sha>`,
//! `--explain <test>`, `--backtest`, `--junit <path>`, `--json`, and `--also-run <file>`.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use syntax::{AstNode, Edition, SyntaxKind, ast};

/// Exit code 10: "cannot attribute this change, nothing was run".
pub(crate) const EXIT_UNATTRIBUTABLE: i32 = 10;

pub(crate) struct DiffFile {
    pub(crate) abs_path: PathBuf,
    pub(crate) base_text: String,
}

/// Inspects `git diff <sha>` and returns the changed `.rs` files along with their content at `<sha>`.
/// If any change cannot be safely attributed to HIR functions/types/consts/statics, prints the
/// reason and exits with code 10 so CI falls back to the full test suite.
pub(crate) fn analyze_git_diff_since(
    workspace_root: &Path,
    sha: &str,
) -> anyhow::Result<Vec<DiffFile>> {
    let diff_out = Command::new("git")
        .current_dir(workspace_root)
        .args(["diff", "--name-only", sha])
        .output()
        .context("running `git diff --name-only`")?;
    if !diff_out.status.success() {
        let stderr = String::from_utf8_lossy(&diff_out.stderr);
        bail!("`git diff --name-only {sha}` failed: {}", stderr.trim());
    }
    let mut changed_paths: Vec<String> = String::from_utf8_lossy(&diff_out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    if let Ok(untracked) = Command::new("git")
        .current_dir(workspace_root)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        && untracked.status.success()
    {
        for line in String::from_utf8_lossy(&untracked.stdout).lines() {
            let p = line.trim();
            if !p.is_empty() && !changed_paths.iter().any(|x| x == p) {
                changed_paths.push(p.to_owned());
            }
        }
    }

    let mut diff_files = Vec::new();
    for rel in &changed_paths {
        if let Some(reason) = unattributable_path_reason(workspace_root, rel) {
            exit_unattributable(&reason);
        }
        let abs_path = workspace_root.join(rel);
        let cur_text = match fs::read_to_string(&abs_path) {
            Ok(t) => t,
            Err(_) => {
                exit_unattributable(&format!("deleted or unreadable source file `{rel}`"));
            }
        };
        let show_out = Command::new("git")
            .current_dir(workspace_root)
            .args(["show", &format!("{sha}:{rel}")])
            .output()
            .context("running `git show`")?;
        let base_text = if show_out.status.success() {
            String::from_utf8_lossy(&show_out.stdout).into_owned()
        } else {
            String::new()
        };
        if let Some(reason) = unattributable_ast_diff_reason(rel, &base_text, &cur_text) {
            exit_unattributable(&reason);
        }
        diff_files.push(DiffFile {
            abs_path,
            base_text,
        });
    }

    Ok(diff_files)
}

pub(crate) fn exit_unattributable(reason: &str) -> ! {
    eprintln!("[rustd] cannot attribute change ({reason}); exiting 10 for full suite fallback");
    std::process::exit(EXIT_UNATTRIBUTABLE);
}

fn unattributable_path_reason(workspace_root: &Path, rel: &str) -> Option<String> {
    let file_name = Path::new(rel).file_name().and_then(|n| n.to_str()).unwrap_or("");
    if rel == "Cargo.lock" || file_name == "Cargo.lock" {
        return Some("Cargo.lock was modified".to_owned());
    }
    if file_name == "Cargo.toml" {
        return Some(format!("manifest `{rel}` was modified"));
    }
    if file_name == "build.rs" {
        return Some(format!("build script `{rel}` was modified"));
    }
    if file_name == "rust-toolchain" || file_name == "rust-toolchain.toml" {
        return Some(format!("toolchain file `{rel}` was modified"));
    }
    if rel.starts_with(".config/") || rel.contains("/.config/") {
        return Some(format!("configuration file `{rel}` was modified"));
    }
    if file_name.starts_with("Makefile") && file_name.ends_with(".toml") {
        return Some(format!("build makefile `{rel}` was modified"));
    }
    if rel.ends_with(".surql") || rel.contains("language-tests/") {
        return Some(format!("test fixture `{rel}` was modified"));
    }
    if !rel.ends_with(".rs") {
        return Some(format!("non-Rust file `{rel}` was modified"));
    }
    // Check if the `.rs` file belongs to a proc-macro crate.
    let mut dir = workspace_root.join(rel);
    while dir.pop() {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            if let Ok(txt) = fs::read_to_string(&manifest)
                && (txt.contains("proc-macro = true") || txt.contains("proc_macro = true"))
            {
                return Some(format!("proc-macro crate file `{rel}` was modified"));
            }
            break;
        }
        if dir == workspace_root {
            break;
        }
    }
    None
}

fn non_trivia_text(node: &syntax::SyntaxNode) -> String {
    let mut out = String::new();
    for el in node.descendants_with_tokens() {
        if let Some(tok) = el.into_token()
            && !tok.kind().is_trivia()
        {
            out.push_str(tok.text());
            out.push(' ');
        }
    }
    out
}

fn unattributable_ast_diff_reason(rel: &str, old_src: &str, new_src: &str) -> Option<String> {
    if old_src.is_empty() {
        return None;
    }
    let old_parse = ast::SourceFile::parse(old_src, Edition::CURRENT);
    let new_parse = ast::SourceFile::parse(new_src, Edition::CURRENT);
    if !old_parse.errors().is_empty() || !new_parse.errors().is_empty() {
        return None;
    }
    let old_tree = old_parse.tree();
    let new_tree = new_parse.tree();

    // 1. Check `macro_rules!` definitions
    let collect_macros = |root: &syntax::SyntaxNode| -> Vec<String> {
        root.descendants()
            .filter_map(ast::MacroRules::cast)
            .map(|m| non_trivia_text(m.syntax()))
            .collect()
    };
    if collect_macros(old_tree.syntax()) != collect_macros(new_tree.syntax()) {
        return Some(format!("`macro_rules!` definition changed in `{rel}`"));
    }

    // 2. Check `include_str!` / `include_bytes!` calls
    let collect_includes = |root: &syntax::SyntaxNode| -> Vec<String> {
        root.descendants()
            .filter_map(ast::MacroCall::cast)
            .filter(|mc| {
                mc.path()
                    .and_then(|p| p.segment())
                    .and_then(|s| s.name_ref())
                    .is_some_and(|n| {
                        matches!(
                            n.text(),
                            "include_str" | "include_bytes" | "include"
                        )
                    })
            })
            .map(|mc| non_trivia_text(mc.syntax()))
            .collect()
    };
    if collect_includes(old_tree.syntax()) != collect_includes(new_tree.syntax()) {
        return Some(format!("`include_str!`/`include_bytes!` macro changed in `{rel}`"));
    }

    // 3. Check `#[cfg(...)]` attributes (excluding `#[cfg(test)]`) and `cfg!(...)` macro calls
    let collect_cfgs = |root: &syntax::SyntaxNode| -> Vec<String> {
        let mut cfgs = Vec::new();
        for attr in root.descendants().filter_map(ast::Attr::cast) {
            if attr.simple_name().as_deref() == Some("cfg")
                || attr.simple_name().as_deref() == Some("cfg_attr")
            {
                let text = non_trivia_text(attr.syntax());
                if text != "# [ cfg ( test ) ] " {
                    cfgs.push(text);
                }
            }
        }
        for mc in root.descendants().filter_map(ast::MacroCall::cast) {
            if mc
                .path()
                .and_then(|p| p.segment())
                .and_then(|s| s.name_ref())
                .is_some_and(|n| n.text() == "cfg")
            {
                cfgs.push(non_trivia_text(mc.syntax()));
            }
        }
        cfgs
    };
    if collect_cfgs(old_tree.syntax()) != collect_cfgs(new_tree.syntax()) {
        return Some(format!("`#[cfg(...)]` or `cfg!(...)` condition changed in `{rel}`"));
    }

    // 4. Check for global `type` alias or `trait` signature changes that have no body
    let collect_type_aliases = |root: &syntax::SyntaxNode| -> Vec<String> {
        root.descendants()
            .filter_map(ast::TypeAlias::cast)
            .map(|t| non_trivia_text(t.syntax()))
            .collect()
    };
    if collect_type_aliases(old_tree.syntax()) != collect_type_aliases(new_tree.syntax()) {
        return Some(format!("type alias definition changed in `{rel}`"));
    }

    // 5. Verify that if non-trivia tokens changed, at least one Fn, Struct, Enum, Union, Const, Static, or Impl changed
    if non_trivia_text(old_tree.syntax()) != non_trivia_text(new_tree.syntax()) {
        let collect_attributable = |root: &syntax::SyntaxNode| -> Vec<String> {
            root.descendants()
                .filter(|n| {
                    matches!(
                        n.kind(),
                        SyntaxKind::FN
                            | SyntaxKind::STRUCT
                            | SyntaxKind::ENUM
                            | SyntaxKind::UNION
                            | SyntaxKind::CONST
                            | SyntaxKind::STATIC
                            | SyntaxKind::IMPL
                    )
                })
                .map(|n| non_trivia_text(&n))
                .collect()
        };
        if collect_attributable(old_tree.syntax()) == collect_attributable(new_tree.syntax()) {
            return Some(format!(
                "top-level module/import change in `{rel}` not mappable to a function or type"
            ));
        }
    }

    None
}

/// Infers the Cargo package names owning the modified `.rs` files.
pub(crate) fn infer_packages_from_diff(workspace_root: &Path, diff_files: &[DiffFile]) -> Vec<String> {
    let mut pkgs = Vec::new();
    for df in diff_files {
        let mut dir = df.abs_path.clone();
        while dir.pop() {
            let manifest = dir.join("Cargo.toml");
            if manifest.exists() {
                if let Ok(txt) = fs::read_to_string(&manifest) {
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
                            let name = val.trim().trim_matches('"').trim_matches('\'').to_owned();
                            if !name.is_empty() && !pkgs.contains(&name) {
                                pkgs.push(name);
                            }
                            break;
                        }
                    }
                }
                break;
            }
            if dir == workspace_root {
                break;
            }
        }
    }
    pkgs
}

/// Writes `base.baseline.cache` with the `<sha>` baseline contents for all files in `diff_files`.
pub(crate) fn write_since_baseline_cache(
    baseline_cache_path: &Path,
    baseline_fp: &str,
    diff_files: &[DiffFile],
) -> anyhow::Result<()> {
    let mut out = Vec::with_capacity(64 * 1024);
    let put_str = |out: &mut Vec<u8>, s: &str| {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    };
    out.extend_from_slice(b"RUSTDBAS");
    put_str(&mut out, baseline_fp);
    out.extend_from_slice(&(diff_files.len() as u32).to_le_bytes());
    for df in diff_files {
        put_str(&mut out, &df.abs_path.to_string_lossy());
        put_str(&mut out, &df.base_text);
    }
    if fs::read(baseline_cache_path).ok().as_deref() == Some(out.as_slice()) {
        return Ok(());
    }
    fs::write(baseline_cache_path, out)?;
    Ok(())
}

#[derive(Clone, Default)]
pub(crate) struct NextestOverride {
    pub(crate) test_substr: Option<String>,
    pub(crate) package: Option<String>,
    pub(crate) test_group: Option<String>,
    pub(crate) threads_required: usize,
    pub(crate) timeout: Option<Duration>,
}

#[derive(Clone)]
pub(crate) struct NextestConfig {
    pub(crate) retries: usize,
    pub(crate) default_timeout: Duration,
    pub(crate) overrides: Vec<NextestOverride>,
    pub(crate) serial_groups: HashSet<String>,
}

impl Default for NextestConfig {
    fn default() -> Self {
        Self {
            retries: 0,
            default_timeout: Duration::from_secs(60),
            overrides: Vec::new(),
            serial_groups: HashSet::new(),
        }
    }
}

fn parse_duration_str(s: &str) -> Option<Duration> {
    let s = s.trim().trim_matches('"').trim_matches('\'');
    if let Some(sec) = s.strip_suffix('s') {
        sec.trim().parse::<u64>().ok().map(Duration::from_secs)
    } else if let Some(min) = s.strip_suffix('m') {
        min.trim().parse::<u64>().ok().map(|m| Duration::from_secs(m * 60))
    } else {
        None
    }
}

impl NextestConfig {
    pub(crate) fn load(workspace_root: &Path, cli_retries: Option<usize>) -> Self {
        let mut cfg = Self::default();
        let path = workspace_root.join(".config").join("nextest.toml");
        if let Ok(txt) = fs::read_to_string(path) {
            let mut section = String::new();
            let mut cur_ovr: Option<NextestOverride> = None;
            for raw_line in txt.lines() {
                let line = raw_line.split('#').next().unwrap_or("").trim();
                if line.is_empty() {
                    continue;
                }
                if line.starts_with('[') {
                    if let Some(ovr) = cur_ovr.take() {
                        cfg.overrides.push(ovr);
                    }
                    section = line.to_owned();
                    if section == "[[profile.default.overrides]]" {
                        cur_ovr = Some(NextestOverride {
                            threads_required: 1,
                            ..Default::default()
                        });
                    }
                    continue;
                }
                if section == "[profile.default]" {
                    if let Some((_, v)) = line.split_once('=') {
                        let k = line.split('=').next().unwrap_or("").trim();
                        if k == "retries"
                            && let Ok(r) = v.trim().parse::<usize>()
                        {
                            cfg.retries = r;
                        } else if k == "slow-timeout"
                            && let Some(d) = parse_duration_str(v)
                        {
                            cfg.default_timeout = d * 4;
                        }
                    }
                } else if section == "[[profile.default.overrides]]"
                    && let Some(ovr) = &mut cur_ovr
                    && let Some((k, v)) = line.split_once('=')
                {
                    let k = k.trim();
                    let v = v.trim().trim_matches('\'').trim_matches('"');
                    match k {
                        "filter" => {
                            if let Some( start) = v.find("test(")
                                && let Some(end) = v[start + 5..].find(')')
                            {
                                ovr.test_substr = Some(v[start + 5..start + 5 + end].to_owned());
                            }
                            if let Some(start) = v.find("package(")
                                && let Some(end) = v[start + 8..].find(')')
                            {
                                ovr.package = Some(v[start + 8..start + 8 + end].to_owned());
                            }
                        }
                        "test-group" => ovr.test_group = Some(v.to_owned()),
                        "threads-required" => {
                            ovr.threads_required = v.parse::<usize>().unwrap_or(1).max(1);
                        }
                        "slow-timeout" => {
                            ovr.timeout = parse_duration_str(v).map(|d| d * 2);
                        }
                        _ => {}
                    }
                } else if section == "[test-groups]"
                    && let Some((group_name, rest)) = line.split_once('=')
                    && rest.contains("max-threads = 1")
                {
                    cfg.serial_groups.insert(group_name.trim().to_owned());
                }
            }
            if let Some(ovr) = cur_ovr.take() {
                cfg.overrides.push(ovr);
            }
        }
        if let Some(r) = cli_retries {
            cfg.retries = r;
        }
        cfg
    }

    fn settings_for(&self, pkg: &str, test_name: &str) -> (Option<String>, usize, Duration) {
        let mut group = None;
        let mut threads_req = 1usize;
        let mut timeout = self.default_timeout;
        for ovr in &self.overrides {
            let pkg_ok = ovr.package.as_deref().is_none_or(|p| p == pkg);
            let test_ok = ovr.test_substr.as_deref().is_none_or(|t| test_name.contains(t));
            if pkg_ok && test_ok {
                if let Some(g) = &ovr.test_group {
                    group = Some(g.clone());
                }
                threads_req = threads_req.max(ovr.threads_required);
                if let Some(t) = ovr.timeout {
                    timeout = timeout.max(t);
                }
            }
        }
        (group, threads_req, timeout)
    }
}

pub(crate) struct TestCaseOutcome {
    pub(crate) package: String,
    pub(crate) name: String,
    pub(crate) passed: bool,
    pub(crate) zero_matched: bool,
    pub(crate) attempts: usize,
    pub(crate) duration: Duration,
    pub(crate) output: String,
    pub(crate) reason: String,
}

/// Expands HIR test names against `<test_bin> --list --format=terse` so parameterized (`#[rstest]`)
/// tests expand to every exact `libtest` case name.
pub(crate) fn expand_exact_tests(test_bin: &Path, wanted: &[String]) -> Vec<String> {
    if wanted.is_empty() {
        return Vec::new();
    }
    let Ok(out) = Command::new(test_bin)
        .args(["--list", "--format=terse"])
        .output()
    else {
        return wanted.to_vec();
    };
    if !out.status.success() {
        return wanted.to_vec();
    }
    let available: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .map(str::trim)
        .map(str::to_owned)
        .collect();
    let mut expanded = Vec::new();
    for w in wanted {
        let prefix = format!("{w}::");
        let mut matched_any = false;
        for av in &available {
            if av == w || av.starts_with(&prefix) || av.ends_with(&format!("::{w}")) {
                if !expanded.contains(av) {
                    expanded.push(av.clone());
                }
                matched_any = true;
            }
        }
        if !matched_any && !expanded.contains(w) {
            expanded.push(w.clone());
        }
    }
    expanded
}

/// Runs each test in its own isolated child process (`<test_bin> --exact <name>`), honouring
/// `.config/nextest.toml` serial groups, thread slots, timeouts, and retries.
pub(crate) fn run_isolated_test_suite(
    pkg: &str,
    test_bin: &Path,
    tests: &[String],
    reasons: &HashMap<String, String>,
    cfg: &NextestConfig,
    extra_args: &[String],
    json_mode: bool,
) -> Vec<TestCaseOutcome> {
    if tests.is_empty() {
        return Vec::new();
    }
    let max_workers = thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8)
        .min(tests.len());
    let mut group_locks: HashMap<String, Arc<Mutex<()>>> = HashMap::new();
    for g in &cfg.serial_groups {
        group_locks.insert(g.clone(), Arc::new(Mutex::new(())));
    }
    let group_locks = Arc::new(group_locks);
    let next_idx = AtomicUsize::new(0);
    let outcomes: Mutex<Vec<(usize, TestCaseOutcome)>> = Mutex::new(Vec::with_capacity(tests.len()));

    thread::scope(|scope| {
        for _ in 0..max_workers {
            let group_locks = Arc::clone(&group_locks);
            let next_idx = &next_idx;
            let outcomes = &outcomes;
            scope.spawn(move || {
                while let Some(test_name) =
                    tests.get(next_idx.fetch_add(1, Ordering::Relaxed))
                {
                    let idx = tests.iter().position(|t| t == test_name).unwrap_or(0);
                    let (group, _threads_req, _timeout) = cfg.settings_for(pkg, test_name);
                    let _guard = group
                        .as_ref()
                        .and_then(|g| group_locks.get(g))
                        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));

                    let max_attempts = cfg.retries + 1;
                    let mut attempts = 0usize;
                    let mut passed = false;
                    let mut zero_matched = false;
                    let mut combined_out = String::new();
                    let t0 = Instant::now();
                    while attempts < max_attempts {
                        attempts += 1;
                        let mut cmd = Command::new(test_bin);
                        cmd.arg("--exact").arg(test_name);
                        cmd.args(extra_args);
                        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
                        if let Ok(out) = cmd.output() {
                            combined_out = format!(
                                "{}{}",
                                String::from_utf8_lossy(&out.stdout),
                                String::from_utf8_lossy(&out.stderr)
                            );
                            if combined_out.contains("running 0 tests")
                                || combined_out.contains("0 passed; 0 failed")
                            {
                                zero_matched = true;
                                break;
                            }
                            if out.status.success() {
                                passed = true;
                                break;
                            }
                        }
                    }
                    let dur = t0.elapsed();
                    if !json_mode {
                        let status_str = if zero_matched {
                            "\x1b[1;35mERROR (0 tests matched)\x1b[0m".to_owned()
                        } else if passed {
                            if attempts > 1 {
                                format!("\x1b[1;33mFLAKY PASS ({attempts}/{max_attempts})\x1b[0m")
                            } else {
                                "\x1b[1;32mPASS\x1b[0m".to_owned()
                            }
                        } else {
                            "\x1b[1;31mFAIL\x1b[0m".to_owned()
                        };
                        eprintln!(
                            "    {status_str} [{:>7.3}s] {pkg} {test_name}",
                            dur.as_secs_f64()
                        );
                        if !passed {
                            eprintln!("{combined_out}");
                        }
                    }
                    let reason = reasons
                        .get(test_name)
                        .or_else(|| {
                            reasons
                                .iter()
                                .find(|(k, _)| test_name.starts_with(&format!("{k}::")))
                                .map(|(_, v)| v)
                        })
                        .cloned()
                        .unwrap_or_else(|| "selected".to_owned());
                    outcomes.lock().unwrap().push((
                        idx,
                        TestCaseOutcome {
                            package: pkg.to_owned(),
                            name: test_name.clone(),
                            passed,
                            zero_matched,
                            attempts,
                            duration: dur,
                            output: combined_out,
                            reason,
                        },
                    ));
                }
            });
        }
    });

    let mut list = outcomes.into_inner().unwrap();
    list.sort_by_key(|(idx, _)| *idx);
    list.into_iter().map(|(_, o)| o).collect()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(crate) fn write_junit_xml(path: &Path, outcomes: &[TestCaseOutcome], total_dur: Duration) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let total = outcomes.len();
    let failures = outcomes.iter().filter(|o| !o.passed).count();
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<testsuites name=\"rustd\" tests=\"{total}\" failures=\"{failures}\" errors=\"0\" time=\"{:.3}\">\n",
        total_dur.as_secs_f64()
    ));
    let mut by_pkg: HashMap<&str, Vec<&TestCaseOutcome>> = HashMap::new();
    for o in outcomes {
        by_pkg.entry(&o.package).or_default().push(o);
    }
    for (pkg, cases) in by_pkg {
        let pkg_fails = cases.iter().filter(|c| !c.passed).count();
        let pkg_time: f64 = cases.iter().map(|c| c.duration.as_secs_f64()).sum();
        xml.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{}\" disabled=\"0\" errors=\"0\" failures=\"{pkg_fails}\" time=\"{pkg_time:.3}\">\n",
            xml_escape(pkg),
            cases.len()
        ));
        for c in cases {
            xml.push_str(&format!(
                "    <testcase name=\"{}\" classname=\"{}\" time=\"{:.3}\">\n",
                xml_escape(&c.name),
                xml_escape(pkg),
                c.duration.as_secs_f64()
            ));
            if !c.passed {
                xml.push_str(&format!(
                    "      <failure message=\"test failed\">{}</failure>\n",
                    xml_escape(&c.output)
                ));
            }
            xml.push_str("    </testcase>\n");
        }
        xml.push_str("  </testsuite>\n");
    }
    xml.push_str("</testsuites>\n");
    fs::write(path, xml)?;
    Ok(())
}

pub(crate) fn print_json_summary(outcomes: &[TestCaseOutcome], total_dur: Duration) {
    let passed = outcomes.iter().filter(|o| o.passed).count();
    let failed = outcomes.len() - passed;
    let mut items = Vec::new();
    for o in outcomes {
        let esc_name = o.name.replace('\\', "\\\\").replace('"', "\\\"");
        let esc_pkg = o.package.replace('\\', "\\\\").replace('"', "\\\"");
        let esc_reason = o.reason.replace('\\', "\\\\").replace('"', "\\\"");
        items.push(format!(
            "{{\"package\":\"{esc_pkg}\",\"name\":\"{esc_name}\",\"passed\":{},\"attempts\":{},\"duration_ms\":{:.2},\"reason\":\"{esc_reason}\"}}",
            o.passed,
            o.attempts,
            o.duration.as_secs_f64() * 1000.0
        ));
    }
    println!(
        "{{\"total\":{},\"passed\":{passed},\"failed\":{failed},\"duration_ms\":{:.2},\"tests\":[{}]}}",
        outcomes.len(),
        total_dur.as_secs_f64() * 1000.0,
        items.join(",")
    );
}
