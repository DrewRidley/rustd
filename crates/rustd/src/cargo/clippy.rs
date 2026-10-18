//! Function-level incremental Clippy via `RUSTC_WORKSPACE_WRAPPER`.
//!
//! When a crate of the workspace is checked by `cargo rustd clippy`, `rustd` compares each
//! `.rs` file of the crate against the baseline (`git HEAD` and the last Clippy snapshot) at
//! the AST function level. Functions whose AST is unchanged have their bodies elided to
//! `{ loop {} }` (preserving exact newline counts so diagnostic line numbers never shift) in
//! a shadow directory mapped back to the real paths with `--remap-path-prefix`. `clippy-driver`
//! then typechecks, borrow-checks, and runs its lint passes only on the changed functions.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, Edition, SyntaxKind, SyntaxNode,
    ast::{self, HasAttrs as _, HasName as _, HasVisibility as _},
};

const INERT_ATTRS: &[&str] = &[
    "allow",
    "cfg",
    "cfg_attr",
    "cold",
    "deny",
    "deprecated",
    "doc",
    "expect",
    "forbid",
    "inline",
    "must_use",
    "track_caller",
    "warn",
    "instrument",
];

const INERT_STMT_MACROS: &[&str] = &[
    "println",
    "eprintln",
    "print",
    "eprint",
    "write",
    "writeln",
    "debug",
    "info",
    "warn",
    "error",
    "trace",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "bail",
    "ensure",
    "panic",
    "unreachable",
    "unimplemented",
    "todo",
];

const ALLOW_MODULE: &str = "#![allow(dead_code, unused_imports, unused_macros, unused_qualifications, unfulfilled_lint_expectations, clippy::duplicated_attributes, clippy::non_canonical_partial_ord_impl, clippy::non_canonical_clone_impl)] ";
const ALLOW_LEAF_UNCHANGED: &str = "#![allow(unused, dead_code, unreachable_code, unfulfilled_lint_expectations, clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction, clippy::duplicated_attributes)] ";
const ALLOW_FN: &str = "#[allow(unused, dead_code, unreachable_code, unfulfilled_lint_expectations, clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)] ";

struct BaseFnMap {
    fns: FxHashMap<String, FxHashSet<String>>,
}

impl BaseFnMap {
    fn parse(text: &str) -> Self {
        let tree = syntax::SourceFile::parse(text, Edition::CURRENT).syntax_node();
        let mut fns: FxHashMap<String, FxHashSet<String>> = FxHashMap::default();
        for node in tree.descendants() {
            if node.kind() == SyntaxKind::FN
                && !node.ancestors().skip(1).any(|a| a.kind() == SyntaxKind::BLOCK_EXPR)
            {
                let key = scoped_fn_key(&node);
                let norm = non_trivia_text(&node);
                fns.entry(key).or_default().insert(norm);
            }
        }
        Self { fns }
    }

    fn contains_unchanged(&self, node: &SyntaxNode) -> bool {
        let key = scoped_fn_key(node);
        let Some(set) = self.fns.get(&key) else {
            return false;
        };
        set.contains(&non_trivia_text(node))
    }
}

fn non_trivia_text(node: &SyntaxNode) -> String {
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

fn scoped_fn_key(node: &SyntaxNode) -> String {
    let mut parts = Vec::new();
    for anc in node.ancestors() {
        if let Some(m) = ast::Module::cast(anc.clone()) {
            let name = m.name().map_or_else(|| "_".to_owned(), |n| n.text().to_string());
            parts.push(format!("mod:{name}"));
        } else if let Some(imp) = ast::Impl::cast(anc.clone()) {
            let self_ty = imp
                .self_ty()
                .map_or_else(String::new, |t| non_trivia_text(t.syntax()));
            let trait_ty = imp
                .trait_()
                .map_or_else(String::new, |t| non_trivia_text(t.syntax()));
            parts.push(format!("impl:{self_ty}:{trait_ty}"));
        } else if let Some(tr) = ast::Trait::cast(anc.clone()) {
            let name = tr.name().map_or_else(|| "_".to_owned(), |n| n.text().to_string());
            parts.push(format!("trait:{name}"));
        } else if let Some(f) = ast::Fn::cast(anc) {
            let name = f.name().map_or_else(|| "_".to_owned(), |n| n.text().to_string());
            parts.push(format!("fn:{name}"));
        }
    }
    parts.reverse();
    parts.join("::")
}

fn has_only_inert_attrs(node: &SyntaxNode) -> bool {
    let Some(item) = ast::AnyHasAttrs::cast(node.clone()) else {
        return true;
    };
    item.attrs().all(|attr| {
        let Some(path) = attr.path() else { return false };
        let text = path.syntax().text().to_string();
        let last = text.rsplit("::").next().unwrap_or(&text);
        INERT_ATTRS.contains(&last)
    })
}

fn is_elidable_fn(func: &ast::Fn) -> bool {
    let Some(body) = func.body() else { return false };
    if func.const_token().is_some() || func.gen_token().is_some() {
        return false;
    }
    if func
        .name()
        .is_some_and(|n| matches!(n.text(), "main" | "partial_cmp" | "clone"))
    {
        return false;
    }
    if let Some(rt) = func.ret_type()
        && rt.syntax().descendants().any(|n| {
            matches!(n.kind(), SyntaxKind::IMPL_TRAIT_TYPE | SyntaxKind::MACRO_CALL)
        })
    {
        return false;
    }
    if !has_only_inert_attrs(func.syntax()) {
        return false;
    }
    let ancestors_ok = func.syntax().ancestors().skip(1).all(|anc| match anc.kind() {
        SyntaxKind::SOURCE_FILE | SyntaxKind::ITEM_LIST | SyntaxKind::ASSOC_ITEM_LIST => true,
        SyntaxKind::MODULE | SyntaxKind::IMPL | SyntaxKind::TRAIT => has_only_inert_attrs(&anc),
        _ => false,
    });
    if !ancestors_ok {
        return false;
    }
    let declares_outer = body.syntax().descendants().skip(1).any(|n| match n.kind() {
        SyntaxKind::STRUCT
        | SyntaxKind::ENUM
        | SyntaxKind::UNION
        | SyntaxKind::TRAIT
        | SyntaxKind::IMPL
        | SyntaxKind::TYPE_ALIAS
        | SyntaxKind::MODULE
        | SyntaxKind::EXTERN_BLOCK
        | SyntaxKind::MACRO_RULES
        | SyntaxKind::MACRO_DEF => true,
        SyntaxKind::ATTR => n.ancestors().nth(2).is_some_and(|p| p == *body.syntax()),
        SyntaxKind::MACRO_CALL => {
            let in_expr = n
                .parent()
                .is_some_and(|p| p.kind() == SyntaxKind::MACRO_EXPR);
            if in_expr {
                return false;
            }
            let Some(mc) = ast::MacroCall::cast(n) else { return true };
            let Some(path) = mc.path() else { return true };
            let text = path.syntax().text().to_string();
            let last = text.rsplit("::").next().unwrap_or(&text);
            !INERT_STMT_MACROS.contains(&last)
        }
        _ => false,
    });
    !declares_outer
}

/// Elides unchanged function bodies in `text`. Returns `(rewritten_text, kept_changed_fns, elided_fns)`.
pub(crate) fn elide_unchanged_in_file(
    text: &str,
    baselines: &[String],
) -> (String, usize, usize) {
    let file_unchanged = !baselines.is_empty() && baselines.iter().all(|b| b == text);
    let base_maps: Vec<BaseFnMap> = if file_unchanged {
        Vec::new()
    } else {
        baselines.iter().map(|b| BaseFnMap::parse(b)).collect()
    };

    let tree = syntax::SourceFile::parse(text, Edition::CURRENT).syntax_node();
    // Collect edits as `(pos, len, replacement)` sorted in reverse byte order.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut kept_changed = 0usize;
    let mut elided = 0usize;

    for node in tree.descendants() {
        let Some(func) = ast::Fn::cast(node.clone()) else { continue };
        if node.ancestors().skip(1).any(|a| a.kind() == SyntaxKind::BLOCK_EXPR) {
            continue;
        }
        let unchanged = if file_unchanged {
            true
        } else {
            !base_maps.is_empty() && base_maps.iter().all(|m| m.contains_unchanged(&node))
        };
        if !unchanged {
            kept_changed += 1;
            continue;
        }
        if !is_elidable_fn(&func) {
            continue;
        }
        let Some(body) = func.body() else { continue };
        let range = body.syntax().text_range();
        let (start, end) = (usize::from(range.start()), usize::from(range.end()));
        let Some(slice) = text.get(start..end) else { continue };
        let newlines = slice.bytes().filter(|&b| b == b'\n').count();
        let replacement = format!("{{ loop {{}}{} }}", "\n".repeat(newlines));
        edits.push((start, end - start, replacement));
        let fn_kw_pos = func
            .visibility()
            .map(|v| v.syntax().text_range().start())
            .or_else(|| func.default_token().map(|t| t.text_range().start()))
            .or_else(|| func.const_token().map(|t| t.text_range().start()))
            .or_else(|| func.async_token().map(|t| t.text_range().start()))
            .or_else(|| func.unsafe_token().map(|t| t.text_range().start()))
            .or_else(|| func.abi().map(|a| a.syntax().text_range().start()))
            .or_else(|| func.fn_token().map(|t| t.text_range().start()));
        if let Some(pos) = fn_kw_pos {
            edits.push((usize::from(pos), 0, ALLOW_FN.to_owned()));
        }
        elided += 1;
    }

    edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut out = text.to_owned();
    for (pos, len, rep) in edits {
        out.replace_range(pos..pos + len, &rep);
    }
    let has_child_mods = tree.descendants().any(|n| {
        ast::Module::cast(n).is_some_and(|m| m.item_list().is_none())
    });
    let header = if file_unchanged && !has_child_mods {
        ALLOW_LEAF_UNCHANGED
    } else {
        ALLOW_MODULE
    };
    let insert_at = if out.starts_with("#!") && !out.starts_with("#![") {
        out.find('\n').map_or(0, |i| i + 1)
    } else {
        0
    };
    out.insert_str(insert_at, header);
    (out, kept_changed, elided)
}

/// Returns git-changed files relative to `HEAD` in `cwd`, and a helper to read `HEAD:<rel>`.
fn git_changed_files(cwd: &Path) -> Option<FxHashSet<PathBuf>> {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["diff", "--name-only", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut set = FxHashSet::default();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p = line.trim();
        if !p.is_empty() {
            set.insert(cwd.join(p));
        }
    }
    if let Ok(untracked) = Command::new("git")
        .current_dir(cwd)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        && untracked.status.success()
    {
        for line in String::from_utf8_lossy(&untracked.stdout).lines() {
            let p = line.trim();
            if !p.is_empty() {
                set.insert(cwd.join(p));
            }
        }
    }
    Some(set)
}

fn git_head_text(cwd: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(cwd).ok()?;
    let out = Command::new("git")
        .current_dir(cwd)
        .arg("show")
        .arg(format!("HEAD:{}", rel.display()))
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn mirror_non_rs_files(src_dir: &Path, dst_dir: &Path) -> std::io::Result<()> {
    let Ok(entries) = fs::read_dir(src_dir) else { return Ok(()) };
    fs::create_dir_all(dst_dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let dst = dst_dir.join(&name);
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            mirror_non_rs_files(&path, &dst)?;
        } else if path.extension().is_none_or(|e| e != "rs") && !dst.exists() {
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(&path, &dst);
            #[cfg(not(unix))]
            let _ = fs::copy(&path, &dst);
        }
    }
    Ok(())
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

pub(crate) struct ShadowClippy {
    pub(crate) shadow_root: PathBuf,
    pub(crate) shadow_source: PathBuf,
    pub(crate) kept_changed_fns: usize,
    pub(crate) elided_fns: usize,
}

/// Builds a function-elided shadow tree for `unit` if a baseline (snapshot or git HEAD) is
/// available.
pub(crate) fn prepare_shadow(
    cwd: &Path,
    source: &Path,
    unit_dir: &Path,
    known_files: &[PathBuf],
) -> Option<ShadowClippy> {
    if std::env::var_os("RUSTD_FULL_CLIPPY").is_some() {
        return None;
    }
    let package_src_dir = source.parent()?;
    let mut files: Vec<PathBuf> = known_files
        .iter()
        .filter(|p| p.starts_with(cwd) && p.extension().is_some_and(|e| e == "rs"))
        .cloned()
        .collect();
    if files.is_empty() {
        collect_rs_files(package_src_dir, &mut files);
    }
    if files.is_empty() {
        return None;
    }

    let snapshot_dir = unit_dir.join("clippy-snapshot");
    let has_snapshot = snapshot_dir.is_dir();
    let git_changed = git_changed_files(cwd);
    if !has_snapshot && git_changed.is_none() {
        return None;
    }

    let shadow_root = unit_dir.join("shadow");
    let _ = fs::remove_dir_all(&shadow_root);
    fs::create_dir_all(&shadow_root).ok()?;
    if let Some(pkg_dir) = package_src_dir.parent()
        && pkg_dir.starts_with(cwd)
        && let Ok(rel_pkg) = pkg_dir.strip_prefix(cwd)
    {
        let _ = mirror_non_rs_files(pkg_dir, &shadow_root.join(rel_pkg));
    }

    let mut total_kept = 0usize;
    let mut total_elided = 0usize;

    for file in &files {
        let Ok(rel) = file.strip_prefix(cwd) else { continue };
        let Ok(text) = fs::read_to_string(file) else { continue };
        let mut baselines = Vec::new();
        if has_snapshot
            && let Ok(snap) = fs::read_to_string(snapshot_dir.join(rel))
        {
            baselines.push(snap);
        }
        if let Some(ref changed_set) = git_changed {
            if changed_set.contains(file) {
                if let Some(head) = git_head_text(cwd, file)
                    && !baselines.contains(&head)
                {
                    baselines.push(head);
                }
            } else if baselines.is_empty() {
                baselines.push(text.clone());
            }
        }
        let (rewritten, kept, elided) = elide_unchanged_in_file(&text, &baselines);
        total_kept += kept;
        total_elided += elided;
        let dst = shadow_root.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).ok()?;
        }
        fs::write(&dst, rewritten).ok()?;
    }

    if total_elided == 0 {
        return None;
    }

    let rel_source = source.strip_prefix(cwd).ok()?;
    let shadow_source = shadow_root.join(rel_source);
    shadow_source.exists().then_some(ShadowClippy {
        shadow_root,
        shadow_source,
        kept_changed_fns: total_kept,
        elided_fns: total_elided,
    })
}

/// Saves the current `.rs` files of `unit` as the baseline snapshot for the next run.
pub(crate) fn save_snapshot(cwd: &Path, unit_dir: &Path, files: &[PathBuf]) {
    let snapshot_dir = unit_dir.join("clippy-snapshot");
    for file in files {
        let Ok(rel) = file.strip_prefix(cwd) else { continue };
        if file.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let dst = snapshot_dir.join(rel);
        if let Some(parent) = dst.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::copy(file, dst);
    }
}
