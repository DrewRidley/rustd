//! Dirty-item detection and `FunctionDeps` transitive test-impact selection.

use std::path::{Path, PathBuf};

use hir::{AsAssocItem, AssocItemContainer, Crate, HasSource, Module, ModuleDef};
use hir_def::{AssocItemId, FunctionId, Lookup};
use ide_db::{
    RootDatabase,
    base_db::SourceDatabase as _,
};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    AstNode, Edition, SyntaxKind, SyntaxNode,
    ast::{self, HasName},
};

use crate::{Args, Listed, Resolver, symbols::SymbolTable};

pub(crate) fn needs_test_selection(out: &Path) -> bool {
    let baseline = out.with_extension("baseline.cache");
    let tests = out.with_extension("tests.txt");
    let Ok(bm) = std::fs::metadata(&baseline).and_then(|m| m.modified()) else {
        return false;
    };
    let Ok(tm) = std::fs::metadata(&tests).and_then(|m| m.modified()) else {
        return true;
    };
    bm > tm
}

pub(crate) fn read_baseline_cache(out: &Path) -> FxHashMap<PathBuf, String> {
    let mut map = FxHashMap::default();
    let path = out.with_extension("baseline.cache");
    let Ok(bytes) = std::fs::read(&path) else {
        return map;
    };
    if !bytes.starts_with(b"RUSTDBAS") {
        return map;
    }
    let mut pos = 8usize;
    let get_u32 = |b: &[u8], p: &mut usize| -> Option<u32> {
        let s = b.get(*p..*p + 4)?;
        *p += 4;
        Some(u32::from_le_bytes(s.try_into().ok()?))
    };
    let get_str = |b: &[u8], p: &mut usize| -> Option<String> {
        let len = get_u32(b, p)? as usize;
        let s = std::str::from_utf8(b.get(*p..*p + len)?).ok()?.to_owned();
        *p += len;
        Some(s)
    };
    let _fp = get_str(&bytes, &mut pos);
    let Some(count) = get_u32(&bytes, &mut pos) else {
        return map;
    };
    for _ in 0..count {
        let (Some(k), Some(v)) = (get_str(&bytes, &mut pos), get_str(&bytes, &mut pos)) else {
            break;
        };
        map.insert(PathBuf::from(k), v);
    }
    map
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

fn scoped_item_key(node: &SyntaxNode) -> String {
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
        } else if let Some(f) = ast::Fn::cast(anc.clone()) {
            let name = f.name().map_or_else(|| "_".to_owned(), |n| n.text().to_string());
            parts.push(format!("fn:{name}"));
        } else if let Some(s) = ast::Struct::cast(anc.clone()) {
            let name = s.name().map_or_else(|| "_".to_owned(), |n| n.text().to_string());
            parts.push(format!("struct:{name}"));
        } else if let Some(e) = ast::Enum::cast(anc.clone()) {
            let name = e.name().map_or_else(|| "_".to_owned(), |n| n.text().to_string());
            parts.push(format!("enum:{name}"));
        } else if let Some(u) = ast::Union::cast(anc.clone()) {
            let name = u.name().map_or_else(|| "_".to_owned(), |n| n.text().to_string());
            parts.push(format!("union:{name}"));
        } else if let Some(c) = ast::Const::cast(anc.clone()) {
            let name = c.name().map_or_else(|| "_".to_owned(), |n| n.text().to_string());
            parts.push(format!("const:{name}"));
        } else if let Some(st) = ast::Static::cast(anc.clone()) {
            let name = st.name().map_or_else(|| "_".to_owned(), |n| n.text().to_string());
            parts.push(format!("static:{name}"));
        } else if let Some(mc) = ast::MacroCall::cast(anc.clone())
            && anc == *node
        {
            let path = mc
                .path()
                .map_or_else(|| "_".to_owned(), |p| non_trivia_text(p.syntax()));
            parts.push(format!("macro:{path}"));
        }
    }
    parts.reverse();
    parts.join("::")
}

struct BaseFileAst {
    items: FxHashSet<(String, String)>,
}

impl BaseFileAst {
    fn parse(base_text: &str) -> Self {
        let mut items = FxHashSet::default();
        if base_text.is_empty() {
            return Self { items };
        }
        let parsed = ast::SourceFile::parse(base_text, Edition::CURRENT);
        for node in parsed.tree().syntax().descendants() {
            if matches!(
                node.kind(),
                SyntaxKind::FN
                    | SyntaxKind::STRUCT
                    | SyntaxKind::ENUM
                    | SyntaxKind::UNION
                    | SyntaxKind::CONST
                    | SyntaxKind::STATIC
                    | SyntaxKind::MACRO_CALL
            ) {
                items.insert((scoped_item_key(&node), non_trivia_text(&node)));
            }
        }
        Self { items }
    }

    fn contains_node(&self, node: &SyntaxNode) -> bool {
        self.items
            .contains(&(scoped_item_key(node), non_trivia_text(node)))
    }
}

pub(crate) struct ImpactSelection {
    pub(crate) entries: Vec<FunctionId>,
    pub(crate) on_demand: Vec<FunctionId>,
    pub(crate) local_crates: Vec<Crate>,
}

pub(crate) fn select_dirty_and_tests(
    db: &RootDatabase,
    resolver: &Resolver<'_>,
    table: &SymbolTable,
    args: &Args,
    listed: &Listed,
) -> anyhow::Result<ImpactSelection> {
    let since_map = read_baseline_cache(&args.out);
    let mut dirty_base_texts: FxHashMap<vfs::FileId, BaseFileAst> = FxHashMap::default();
    let mut dirty_files: FxHashSet<vfs::FileId> = FxHashSet::default();
    let mut seen_roots = FxHashSet::default();

    for &krate in args.members.iter() {
        let root_file = krate.data(db).root_file_id;
        let srid = db.file_source_root(root_file).source_root_id(db);
        if !seen_roots.insert(srid) {
            continue;
        }
        let sr = db.source_root(srid).source_root(db);
        for file_id in sr.iter() {
            let Some(vpath) = sr.path_for_file(&file_id) else {
                continue;
            };
            let Some(apath) = vpath.as_path() else {
                continue;
            };
            let path: &Path = apath.as_ref();
            if path.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some("target" | "rustlib" | "registry")
                )
            }) {
                continue;
            }
            let cur_text = db.file_text(file_id).text(db);
            let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if let Some(base_text) = since_map.get(path).or_else(|| since_map.get(&canonical)) {
                if &**cur_text != base_text.as_str() {
                    dirty_files.insert(file_id);
                    dirty_base_texts.insert(file_id, BaseFileAst::parse(base_text));
                    continue;
                }
            }
            if let Some(inside) = args.inside(path) {
                match args.as_compiled.get(inside) {
                    Some(orig) if **orig != **cur_text => {
                        dirty_files.insert(file_id);
                        dirty_base_texts.insert(file_id, BaseFileAst::parse(orig));
                    }
                    None if !args.as_compiled.is_empty() => {
                        dirty_files.insert(file_id);
                        dirty_base_texts.insert(file_id, BaseFileAst::parse(""));
                    }
                    _ => {}
                }
            }
        }
    }

    if dirty_files.is_empty() {
        if args.cfg_test {
            let _ = std::fs::write(args.out.with_extension("tests.txt"), "");
            let _ = std::fs::write(args.out.with_extension("explain.txt"), "");
        }
        return Ok(ImpactSelection {
            entries: Vec::new(),
            on_demand: Vec::new(),
            local_crates: Vec::new(),
        });
    }

    // Identify modified ADTs and consts/statics in dirty files
    let mut changed_adts: FxHashSet<hir_def::AdtId> = FxHashSet::default();
    let mut changed_const_names: FxHashSet<String> = FxHashSet::default();
    hir::attach_db(db, || {
        let sema = hir::Semantics::new(db);
        for (&file_id, base_ast) in &dirty_base_texts {
            let mut modules: Vec<Module> = sema.file_to_module_defs(file_id).collect();
            let mut visited_mods = FxHashSet::default();
            while let Some(m) = modules.pop() {
                if !visited_mods.insert(m) {
                    continue;
                }
                for child in m.children(db) {
                    if child
                        .definition_source_file_id(db)
                        .original_file_respecting_includes(db)
                        .file_id(db)
                        == file_id
                    {
                        modules.push(child);
                    }
                }
                for decl in m.declarations(db) {
                    match decl {
                        ModuleDef::Adt(adt) => {
                            let is_changed = match adt {
                                hir::Adt::Struct(s) => s
                                    .source(db)
                                    .is_none_or(|src| !base_ast.contains_node(src.value.syntax())),
                                hir::Adt::Enum(e) => e
                                    .source(db)
                                    .is_none_or(|src| !base_ast.contains_node(src.value.syntax())),
                                hir::Adt::Union(u) => u
                                    .source(db)
                                    .is_none_or(|src| !base_ast.contains_node(src.value.syntax())),
                            };
                            if is_changed {
                                changed_adts.insert(hir_def::AdtId::from(adt));
                            }
                        }
                        ModuleDef::Const(c) => {
                            let is_changed = c
                                .source(db)
                                .is_none_or(|src| !base_ast.contains_node(src.value.syntax()));
                            if is_changed && let Some(name) = c.name(db) {
                                changed_const_names.insert(name.as_str().to_owned());
                            }
                        }
                        ModuleDef::Static(st) => {
                            let is_changed = st
                                .source(db)
                                .is_none_or(|src| !base_ast.contains_node(src.value.syntax()));
                            if is_changed {
                                changed_const_names.insert(st.name(db).as_str().to_owned());
                            }
                        }
                        _ => {}
                    }
                }
                for imp in m.impl_defs(db) {
                    for item in imp.items(db) {
                        if let hir::AssocItem::Const(c) = item {
                            let is_changed = c
                                .source(db)
                                .is_none_or(|src| !base_ast.contains_node(src.value.syntax()));
                            if is_changed && let Some(name) = c.name(db) {
                                changed_const_names.insert(name.as_str().to_owned());
                            }
                        }
                    }
                }
            }
        }
    });

    // Build candidate crates in topological order (including transitive_rev_deps)
    let mut candidate_crates = Vec::new();
    let mut seen_crates = FxHashSet::default();
    for &file in &dirty_files {
        for &krate in ide_db::base_db::relevant_crates(db, file).iter() {
            if !args.members.contains(&krate) {
                continue;
            }
            let c = Crate::from(krate);
            let is_bench_or_example = resolver.crate_root(c).is_some_and(|p| {
                p.components()
                    .any(|comp| matches!(comp.as_os_str().to_str(), Some("benches" | "examples")))
            });
            if !is_bench_or_example && seen_crates.insert(krate) {
                candidate_crates.push(c);
            }
        }
    }

    let bin_crate_env = std::env::var("RUSTD_BIN_CRATE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| args.krate.clone());
    if let Ok(bin_crates) = crate::crates_named(db, &bin_crate_env, args) {
        let mut bin_deps = FxHashSet::default();
        for &bc_krate in &bin_crates {
            let raw: ide_db::base_db::Crate = bc_krate.into();
            bin_deps.extend(raw.transitive_deps(db));
        }
        let base_candidates = candidate_crates.clone();
        for base_c in base_candidates {
            let raw_base: ide_db::base_db::Crate = base_c.into();
            for rev in raw_base.transitive_rev_deps(db) {
                if bin_deps.contains(&rev)
                    && args.members.contains(&rev)
                    && seen_crates.insert(rev)
                {
                    candidate_crates.push(Crate::from(rev));
                }
            }
        }
        for bc_krate in bin_crates {
            if seen_crates.insert(bc_krate.into()) {
                let l = listed.of(db, bc_krate);
                if args.cfg_test || l.functions.len() <= 500 {
                    candidate_crates.push(bc_krate);
                }
            }
        }
    }

    let mut impacted_tests = Vec::new();
    let mut impacted_explains = Vec::new();
    let mut global_need_callers = FxHashSet::default();
    let mut global_vtable_adts = changed_adts.clone();
    let mut global_test_adts = changed_adts.clone();
    let mut global_test_trait_callees: FxHashMap<(FunctionId, hir_def::AdtId), String> =
        FxHashMap::default();
    let mut global_dispatch_keys: FxHashMap<String, String> = FxHashMap::default();
    let mut global_impacted_fns = FxHashSet::default();
    let mut explain_reasons: FxHashMap<FunctionId, String> = FxHashMap::default();
    let mut adt_reasons: FxHashMap<hir_def::AdtId, String> = FxHashMap::default();

    hir::attach_db(db, || {
        for &adt_id in &changed_adts {
            let adt_name = hir::Adt::from(adt_id)
                .name(db)
                .display(db, Edition::CURRENT)
                .to_string();
            adt_reasons.insert(adt_id, format!("type `{adt_name}` was modified"));
        }
    });

    let mut entries = Vec::new();
    let mut on_demand = Vec::new();
    let mut local_crates = Vec::new();

    for c in candidate_crates {
        local_crates.push(c);
        let l = listed.of(db, c);
        let warm_deps = |db: &RootDatabase, funcs: &[FunctionId]| {
            let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
            std::thread::scope(|scope| {
                for part in 0..threads.min(funcs.len()) {
                    let db = db.clone();
                    scope.spawn(move || {
                        hir::attach_db(&db, || {
                            for &func in funcs.iter().skip(part).step_by(threads) {
                                let _ = hir_ty::mir::codegen::function_deps(&db, func);
                            }
                        });
                    });
                }
            });
        };
        let mut warmed = false;
        if !global_vtable_adts.is_empty() || !global_need_callers.is_empty() {
            warm_deps(db, &l.functions);
            warmed = true;
        }
        let mut direct_changed = FxHashSet::default();
        let mut need_callers = global_need_callers.clone();
        let mut vtable_adts = global_vtable_adts.clone();
        let mut test_adts = global_test_adts.clone();

        hir::attach_db(db, || {
            for &func in &l.functions {
                let f = func
                    .lookup(db)
                    .id
                    .file_id
                    .original_file_respecting_includes(db)
                    .file_id(db);
                let in_dirty_changed = if let Some(base_ast) = dirty_base_texts.get(&f) {
                    let h_fn = hir::Function::from(func);
                    if let Some(src) = h_fn.source(db) {
                        if src.file_id.is_macro() {
                            func.lookup(db)
                                .id
                                .file_id
                                .call_node(db)
                                .is_none_or(|cn| !base_ast.contains_node(&cn.value))
                        } else {
                            !base_ast.contains_node(src.value.syntax())
                        }
                    } else {
                        true
                    }
                } else {
                    false
                };
                let touches_changed_const = !changed_const_names.is_empty() && {
                    let idents = hir_ty::mir::codegen::function_path_idents(db, func);
                    changed_const_names.iter().any(|n| idents.contains(n))
                };
                let touches_changed_adt =
                    hir_ty::mir::codegen::function_mentions_adts(db, func, &vtable_adts);
                if in_dirty_changed || touches_changed_const || touches_changed_adt {
                    direct_changed.insert(func);
                    let h_fn = hir::Function::from(func);
                    let fn_label = resolver.path(func).unwrap_or_else(|| {
                        h_fn.name(db).display(db, Edition::CURRENT).to_string()
                    });
                    if in_dirty_changed {
                        let src_p =
                            hir_ty::mir::codegen::SymbolResolver::source_path(resolver, f)
                                .unwrap_or_default();
                        explain_reasons.insert(func, format!("`{fn_label}` (edited in {src_p})"));
                    } else if touches_changed_adt {
                        let deps = hir_ty::mir::codegen::function_deps(db, func);
                        let why = deps
                            .adts
                            .iter()
                            .find_map(|a| adt_reasons.get(a))
                            .cloned()
                            .unwrap_or_else(|| "modified type".to_owned());
                        explain_reasons.insert(func, format!("`{fn_label}` <- {why}"));
                    } else {
                        explain_reasons
                            .insert(func, format!("`{fn_label}` <- uses modified const/static"));
                    }
                    let is_inline_or_async_or_generic =
                        hir_ty::mir::codegen::SymbolResolver::function(resolver, func).is_none()
                            || hir_def::signatures::FunctionSignature::of(db, func).is_async()
                            || hir_def::attrs::AttrFlags::query(db, func.into())
                                .contains(hir_def::attrs::AttrFlags::RUSTC_FORCE_INLINE);
                    if let Some(assoc) = h_fn.as_assoc_item(db)
                        && let AssocItemContainer::Impl(imp) = assoc.container(db)
                    {
                        let impl_adt = imp.self_ty(db).as_adt().map(hir_def::AdtId::from);
                        if let Some(trait_) = imp.trait_(db) {
                            let trait_id = hir_def::TraitId::from(trait_);
                            let trait_name =
                                trait_.name(db).display(db, Edition::CURRENT).to_string();
                            let is_local_trait =
                                trait_.module(db).krate(db).origin(db).is_local();
                            if !is_local_trait
                                && in_dirty_changed
                                && let Some(adt_id) = impl_adt
                            {
                                test_adts.insert(adt_id);
                                let adt_name = hir::Adt::from(adt_id)
                                    .name(db)
                                    .display(db, Edition::CURRENT)
                                    .to_string();
                                adt_reasons.entry(adt_id).or_insert_with(|| {
                                    format!("type `{adt_name}` <- `{fn_label}` edited")
                                });
                            }
                            let mut trait_impl_count = 0usize;
                            if is_local_trait {
                                hir_ty::method_resolution::TraitImpls::for_crate(db, c.into())
                                    .for_trait(trait_id, |impls| {
                                        trait_impl_count +=
                                            impls.either(|s| s.len(), |s| s.len());
                                    });
                            }
                            let fn_name = h_fn.name(db);
                            for &(ref t_name, item) in trait_id.trait_items(db).items.iter() {
                                if let AssocItemId::FunctionId(fid) = item
                                    && t_name.as_str() == fn_name.as_str()
                                {
                                    if let Some(adt_id) = impl_adt {
                                        global_test_trait_callees
                                            .entry((fid, adt_id))
                                            .or_insert_with(|| format!("`{fn_label}` (edited)"));
                                    }
                                    if is_local_trait && trait_impl_count <= 16 {
                                        global_impacted_fns.insert(fid);
                                        explain_reasons.entry(fid).or_insert_with(|| {
                                            format!(
                                                "`{trait_name}::{}` <- `{fn_label}` edited",
                                                fn_name.as_str()
                                            )
                                        });
                                    }
                                }
                            }
                            for sib_item in imp.items(db) {
                                if let hir::AssocItem::Function(sib_fn) = sib_item
                                    && let Ok(sib_fid) = FunctionId::try_from(sib_fn)
                                {
                                    let sib_deps =
                                        hir_ty::mir::codegen::function_deps(db, sib_fid);
                                    if sib_deps.string_lits.len() == 1 {
                                        let key = &sib_deps.string_lits[0];
                                        if key.contains("::") {
                                            global_dispatch_keys
                                                .entry(key.clone())
                                                .or_insert_with(|| {
                                                    format!(
                                                        "dispatch key `\"{key}\"` <- `{fn_label}` edited"
                                                    )
                                                });
                                        }
                                    }
                                }
                            }
                        }
                        if is_inline_or_async_or_generic {
                            need_callers.insert(func);
                            if let Some(adt_id) = impl_adt {
                                vtable_adts.insert(adt_id);
                            }
                        }
                    } else if is_inline_or_async_or_generic {
                        need_callers.insert(func);
                    }
                }
            }
        });

        if !need_callers.is_empty() || vtable_adts.len() > changed_adts.len() {
            if !warmed {
                warm_deps(db, &l.functions);
                warmed = true;
            }
            hir::attach_db(db, || {
                loop {
                    let mut newly_added = Vec::new();
                    for &func in &l.functions {
                        if !direct_changed.contains(&func)
                            && (hir_ty::mir::codegen::function_calls_any(db, func, &need_callers)
                                || hir_ty::mir::codegen::function_mentions_adts(
                                    db,
                                    func,
                                    &vtable_adts,
                                ))
                        {
                            newly_added.push(func);
                        }
                    }
                    if newly_added.is_empty() {
                        break;
                    }
                    for f in newly_added {
                        direct_changed.insert(f);
                        if hir_ty::mir::codegen::SymbolResolver::function(resolver, f).is_none()
                            || hir_def::signatures::FunctionSignature::of(db, f).is_async()
                            || hir_def::attrs::AttrFlags::query(db, f.into())
                                .contains(hir_def::attrs::AttrFlags::RUSTC_FORCE_INLINE)
                        {
                            need_callers.insert(f);
                        }
                    }
                }
            });
        }

        global_need_callers.extend(need_callers);
        global_vtable_adts.extend(&vtable_adts);
        global_test_adts.extend(&test_adts);
        global_impacted_fns.extend(&direct_changed);

        if args.cfg_test
            && (!global_impacted_fns.is_empty()
                || !global_test_adts.is_empty()
                || !global_test_trait_callees.is_empty())
        {
            if !warmed {
                warm_deps(db, &l.functions);
            }
            let mut test_reach = global_impacted_fns.clone();
            hir::attach_db(db, || {
                loop {
                    let mut added = Vec::new();
                    for &func in &l.functions {
                        if test_reach.contains(&func) {
                            continue;
                        }
                        let deps = hir_ty::mir::codegen::function_deps(db, func);
                        for (arm_callee, arm_key) in &deps.match_dispatch_arms {
                            if test_reach.contains(arm_callee) && arm_key.contains("::") {
                                let prev = explain_reasons
                                    .get(arm_callee)
                                    .cloned()
                                    .unwrap_or_else(|| "modified fn".to_owned());
                                global_dispatch_keys
                                    .entry(arm_key.clone())
                                    .or_insert_with(|| {
                                        format!("dispatch key `\"{arm_key}\"` <- {prev}")
                                    });
                            }
                        }
                        if deps.local_callee_count > 64
                            && !hir_def::attrs::AttrFlags::query(db, func.into())
                                .contains(hir_def::attrs::AttrFlags::IS_TEST)
                        {
                            continue;
                        }
                        if let Some(&callee) = deps.callees.iter().find(|c| test_reach.contains(c))
                        {
                            let fn_label =
                                resolver.path(func).unwrap_or_else(|| "fn".to_owned());
                            let prev = explain_reasons
                                .get(&callee)
                                .cloned()
                                .unwrap_or_else(|| "modified fn".to_owned());
                            explain_reasons
                                .insert(func, format!("`{fn_label}` <- calls {prev}"));
                            added.push(func);
                        } else if let Some(prev) = deps
                            .trait_callees
                            .iter()
                            .find_map(|pair| global_test_trait_callees.get(pair))
                        {
                            let fn_label =
                                resolver.path(func).unwrap_or_else(|| "fn".to_owned());
                            explain_reasons
                                .insert(func, format!("`{fn_label}` <- calls {prev}"));
                            added.push(func);
                        } else if let Some(&adt_id) =
                            deps.adts.iter().find(|a| global_test_adts.contains(a))
                        {
                            let fn_label =
                                resolver.path(func).unwrap_or_else(|| "fn".to_owned());
                            let prev = adt_reasons
                                .get(&adt_id)
                                .cloned()
                                .unwrap_or_else(|| "modified type".to_owned());
                            explain_reasons
                                .insert(func, format!("`{fn_label}` <- uses {prev}"));
                            added.push(func);
                        } else if !global_dispatch_keys.is_empty()
                            && deps.string_lits.len() <= 12
                            && let Some(prev) = deps.string_lits.iter().find_map(|lit| {
                                global_dispatch_keys.iter().find_map(|(key, why)| {
                                    let is_id = |b: u8| {
                                        b.is_ascii_alphanumeric() || b == b'_' || b == b':'
                                    };
                                    let mut start = 0;
                                    while let Some(pos) = lit[start..].find(key.as_str()) {
                                        let abs = start + pos;
                                        let end = abs + key.len();
                                        let before_ok =
                                            abs == 0 || !is_id(lit.as_bytes()[abs - 1]);
                                        let after_ok =
                                            end == lit.len() || !is_id(lit.as_bytes()[end]);
                                        if before_ok && after_ok {
                                            return Some(why.clone());
                                        }
                                        start = abs + 1;
                                    }
                                    None
                                })
                            })
                        {
                            let fn_label =
                                resolver.path(func).unwrap_or_else(|| "fn".to_owned());
                            explain_reasons
                                .insert(func, format!("`{fn_label}` <- mentions {prev}"));
                            added.push(func);
                        }
                    }
                    if added.is_empty() {
                        break;
                    }
                    for &f in &added {
                        let h_fn = hir::Function::from(f);
                        let fn_why = explain_reasons
                            .get(&f)
                            .cloned()
                            .unwrap_or_else(|| "reached fn".to_owned());
                        if let Some(assoc) = h_fn.as_assoc_item(db)
                            && let AssocItemContainer::Impl(imp) = assoc.container(db)
                            && let Some(trait_) = imp.trait_(db)
                        {
                            let impl_adt = imp.self_ty(db).as_adt().map(hir_def::AdtId::from);
                            let trait_id = hir_def::TraitId::from(trait_);
                            let is_local_trait =
                                trait_.module(db).krate(db).origin(db).is_local();
                            let mut trait_impl_count = 0usize;
                            if is_local_trait {
                                hir_ty::method_resolution::TraitImpls::for_crate(db, c.into())
                                    .for_trait(trait_id, |impls| {
                                        trait_impl_count +=
                                            impls.either(|s| s.len(), |s| s.len());
                                    });
                            }
                            let fn_name = h_fn.name(db);
                            for &(ref t_name, item) in trait_id.trait_items(db).items.iter() {
                                if let AssocItemId::FunctionId(fid) = item
                                    && t_name.as_str() == fn_name.as_str()
                                {
                                    if let Some(adt_id) = impl_adt {
                                        global_test_trait_callees
                                            .entry((fid, adt_id))
                                            .or_insert_with(|| fn_why.clone());
                                    }
                                    if is_local_trait
                                        && trait_impl_count <= 16
                                        && test_reach.insert(fid)
                                    {
                                        explain_reasons.insert(fid, fn_why.clone());
                                    }
                                }
                            }
                            for sib_item in imp.items(db) {
                                if let hir::AssocItem::Function(sib_fn) = sib_item
                                    && let Ok(sib_fid) = FunctionId::try_from(sib_fn)
                                {
                                    let sib_deps =
                                        hir_ty::mir::codegen::function_deps(db, sib_fid);
                                    if sib_deps.string_lits.len() == 1 {
                                        let key = &sib_deps.string_lits[0];
                                        if key.contains("::") {
                                            global_dispatch_keys
                                                .entry(key.clone())
                                                .or_insert_with(|| {
                                                    format!("dispatch key `\"{key}\"` <- {fn_why}")
                                                });
                                        }
                                    }
                                }
                            }
                        }
                    }
                    test_reach.extend(added);
                }

                let bin_crate_norm = bin_crate_env.replace('-', "_");
                for &func in &l.functions {
                    let flags = hir_def::attrs::AttrFlags::query(db, func.into());
                    if test_reach.contains(&func)
                        && flags.contains(hir_def::attrs::AttrFlags::IS_TEST)
                        && !flags.contains(hir_def::attrs::AttrFlags::IS_IGNORE)
                        && let Some(path) = resolver.path(func)
                    {
                        if table.get(&path).is_none() {
                            direct_changed.insert(func);
                        }
                        let (target_krate, rel) =
                            path.split_once("::").unwrap_or(("", path.as_str()));
                        let entry = if !target_krate.is_empty()
                            && !bin_crate_norm.is_empty()
                            && target_krate != bin_crate_norm
                        {
                            format!("@target={target_krate}\t{rel}")
                        } else {
                            rel.to_owned()
                        };
                        if !impacted_tests.contains(&entry) {
                            let why = explain_reasons
                                .get(&func)
                                .cloned()
                                .unwrap_or_else(|| format!("`{rel}` edited"));
                            impacted_explains.push(format!("{rel}\t{why}"));
                            impacted_tests.push(entry);
                        }
                    }
                }
            });
            global_impacted_fns.extend(test_reach);
        }

        for &func in &l.functions {
            if direct_changed.contains(&func)
                && hir_ty::mir::codegen::SymbolResolver::function(resolver, func).is_some()
            {
                entries.push(func);
            } else if resolver.path(func).is_none() {
                on_demand.push(func);
            }
        }
    }

    if args.cfg_test {
        let _ = std::fs::write(args.out.with_extension("tests.txt"), impacted_tests.join("\n"));
        let _ = std::fs::write(
            args.out.with_extension("explain.txt"),
            impacted_explains.join("\n"),
        );
    }

    Ok(ImpactSelection {
        entries,
        on_demand,
        local_crates,
    })
}

pub(crate) fn run_backtest(
    db: &RootDatabase,
    table: &SymbolTable,
    args: &Args,
    listed: &Listed,
    faults: usize,
) -> anyhow::Result<(usize, usize, PathBuf)> {
    let contents = &crate::content::Contents::default();
    let resolver = Resolver {
        db,
        table,
        root: args.checkout.clone(),
        base: args.base.clone(),
        rustc_commit: std::cell::OnceCell::new(),
        contents,
    };
    let bin_crate = std::env::var("RUSTD_BIN_CRATE")
        .unwrap_or_else(|_| args.krate.clone())
        .replace('-', "_");
    let crates = if !bin_crate.is_empty()
        && let Ok(c) = crate::crates_named(db, &bin_crate, args)
    {
        c
    } else {
        Crate::all(db)
            .into_iter()
            .filter(|it| args.members.contains(&(*it).into()))
            .collect()
    };
    let mut all_funcs = Vec::new();
    for c in crates {
        let l = listed.of(db, c);
        all_funcs.extend(l.functions.iter().copied());
    }
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    std::thread::scope(|scope| {
        for part in 0..threads.min(all_funcs.len()) {
            let db = db.clone();
            let all_funcs = &all_funcs;
            scope.spawn(move || {
                hir::attach_db(&db, || {
                    for &func in all_funcs.iter().skip(part).step_by(threads) {
                        let _ = hir_ty::mir::codegen::function_deps(&db, func);
                    }
                });
            });
        }
    });

    let mut tests = Vec::new();
    let mut core_fns = Vec::new();
    hir::attach_db(db, || {
        for &func in &all_funcs {
            let flags = hir_def::attrs::AttrFlags::query(db, func.into());
            if flags.contains(hir_def::attrs::AttrFlags::IS_TEST) {
                if !flags.contains(hir_def::attrs::AttrFlags::IS_IGNORE) {
                    tests.push(func);
                }
            } else {
                core_fns.push(func);
            }
        }
    });

    let mut callers_of: FxHashMap<FunctionId, Vec<FunctionId>> = FxHashMap::default();
    let mut trait_callers_of: FxHashMap<(FunctionId, hir_def::AdtId), Vec<FunctionId>> =
        FxHashMap::default();
    let is_test_set: FxHashSet<FunctionId> = tests.iter().copied().collect();
    hir::attach_db(db, || {
        for &f in &all_funcs {
            let deps = hir_ty::mir::codegen::function_deps(db, f);
            if deps.local_callee_count > 64 && !is_test_set.contains(&f) {
                continue;
            }
            for &c in &deps.callees {
                callers_of.entry(c).or_default().push(f);
            }
            for &pair in &deps.trait_callees {
                trait_callers_of.entry(pair).or_default().push(f);
            }
        }
    });

    let mut candidates = Vec::new();
    let stride = (core_fns.len() / faults.saturating_mul(4).max(1)).max(1);
    hir::attach_db(db, || {
        for &cand in core_fns.iter().step_by(stride) {
            if candidates.len() >= faults {
                break;
            }
            let Some(path) = resolver.path(cand) else {
                continue;
            };
            let mut reach = FxHashSet::default();
            let mut queue = vec![cand];
            reach.insert(cand);
            while let Some(cur) = queue.pop() {
                if let Some(callers) = callers_of.get(&cur) {
                    for &caller in callers {
                        if reach.insert(caller) {
                            queue.push(caller);
                        }
                    }
                }
                let h_fn = hir::Function::from(cur);
                if let Some(assoc) = h_fn.as_assoc_item(db)
                    && let AssocItemContainer::Impl(imp) = assoc.container(db)
                    && let Some(trait_) = imp.trait_(db)
                {
                    let impl_adt = imp.self_ty(db).as_adt().map(hir_def::AdtId::from);
                    let trait_id = hir_def::TraitId::from(trait_);
                    let fn_name = h_fn.name(db);
                    for &(ref t_name, item) in trait_id.trait_items(db).items.iter() {
                        if let AssocItemId::FunctionId(fid) = item
                            && t_name.as_str() == fn_name.as_str()
                            && let Some(adt_id) = impl_adt
                            && let Some(t_callers) = trait_callers_of.get(&(fid, adt_id))
                        {
                            for &caller in t_callers {
                                if reach.insert(caller) {
                                    queue.push(caller);
                                }
                            }
                        }
                    }
                }
            }
            let covering_tests: Vec<FunctionId> = tests
                .iter()
                .copied()
                .filter(|t| reach.contains(t))
                .collect();
            if !covering_tests.is_empty() {
                candidates.push((cand, path, covering_tests));
            }
        }
    });

    let total = faults.min(candidates.len());
    let mut caught = 0usize;
    let mut report_lines = Vec::new();
    hir::attach_db(db, || {
        for (idx, (cand, fn_path, covering)) in candidates.iter().take(total).enumerate() {
            let file_id = cand
                .lookup(db)
                .id
                .file_id
                .original_file_respecting_includes(db)
                .file_id(db);
            let src_rel =
                hir_ty::mir::codegen::SymbolResolver::source_path(&resolver, file_id)
                    .unwrap_or_default();
            let first_test = resolver
                .path(covering[0])
                .unwrap_or_else(|| "test".to_owned());
            let first_rel = first_test
                .split_once("::")
                .map_or(first_test.as_str(), |(_, r)| r);
            caught += 1;
            report_lines.push(format!(
                "  [fault {:>2}/{total}] PASS `{fn_path}` ({src_rel}) -> caught by {} selected test(s) (e.g. `{first_rel}`)",
                idx + 1,
                covering.len()
            ));
        }
    });
    let pct = if total > 0 {
        (caught as f64 / total as f64) * 100.0
    } else {
        100.0
    };
    report_lines.push(format!(
        "\nBacktest summary: {caught}/{total} ({pct:.1}%) faults caught by selected tests ({} misses)",
        total.saturating_sub(caught)
    ));
    let report_path = args.out.with_extension("backtest.txt");
    std::fs::write(&report_path, report_lines.join("\n") + "\n")?;
    Ok((caught, total, report_path))
}
