use crate::*;

/// Resolve the entry point .fai file for a named target. Convention:
/// 1. `main.fai` in the source dir
/// 2. `<target_name>.fai` (e.g. `server.fai` for target "server")
/// 3. Single .fai file if there's only one (ignoring test-* files)
/// The `target_name` hint helps disambiguate when multiple .fai files exist.
pub(crate) fn resolve_entry_point(
    project_root: &std::path::Path,
    source_dir: &str,
) -> Option<std::path::PathBuf> {
    resolve_entry_point_with_hint(project_root, source_dir, None)
}

pub(crate) fn resolve_entry_point_with_hint(
    project_root: &std::path::Path,
    source_dir: &str,
    target_name: Option<&str>,
) -> Option<std::path::PathBuf> {
    let src = project_root.join(source_dir);
    if !src.is_dir() {
        return None;
    }
    // 1. Prefer main.fai
    let main = src.join("main.fai");
    if main.is_file() {
        return Some(main);
    }
    // 2. Try <target_name>.fai or <target_name><anything>.fai
    if let Some(name) = target_name {
        // Exact: server.fai
        let exact = src.join(format!("{}.fai", name));
        if exact.is_file() {
            return Some(exact);
        }
        // Prefix match: todoserver.fai for target "server"
        if let Ok(entries) = std::fs::read_dir(&src) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().map_or(false, |e| e == "fai") {
                    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
                    if stem.ends_with(name) && !stem.starts_with("test") {
                        return Some(p);
                    }
                }
            }
        }
    }
    // 3. Single non-test .fai file
    if let Ok(entries) = std::fs::read_dir(&src) {
        let candidates: Vec<_> = entries
            .flatten()
            .filter(|e| {
                let p = e.path();
                p.extension().map_or(false, |e| e == "fai")
                    && !p
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .starts_with("test")
            })
            .collect();
        if candidates.len() == 1 {
            return Some(candidates[0].path());
        }
    }
    None
}

/// Select which targets to build/run based on command args.
/// Returns (target_name, sub_project) pairs.
/// - No args: all targets (for build) or the single target (for run)
/// - Named arg: just that target
#[cfg(test)]
pub(crate) fn select_targets<'a>(
    info: &'a ProjectInfo,
    target_name: Option<&str>,
) -> Vec<(String, &'a SubProject)> {
    if info.sub_projects.is_empty() {
        // Single-project mode: return a synthetic "default" target
        return vec![];
    }
    match target_name {
        Some(name) => {
            if let Some(sub) = info.sub_projects.get(name) {
                vec![(name.to_string(), sub)]
            } else {
                eprintln!("error: unknown target '{}'. Available targets:", name);
                for k in info.sub_projects.keys() {
                    eprintln!("  - {}", k);
                }
                vec![]
            }
        }
        None => info
            .sub_projects
            .iter()
            .map(|(name, sub)| (name.clone(), sub))
            .collect(),
    }
}

/// Resolve entry point for a named target from the nearest fai.toml.
pub(crate) fn resolve_target_entry_point(target_name: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let root = find_project_root(&cwd)?;
    let toml = std::fs::read_to_string(root.join("fai.toml")).ok()?;
    let info = parse_project_info(&toml);
    let sub = info.sub_projects.get(target_name)?;
    // Prefer explicit main, fall back to convention-based resolution
    if let Some(main) = &sub.main {
        let entry = root.join(main);
        if entry.is_file() {
            return Some(entry.to_string_lossy().into_owned());
        }
    }
    let src = sub.source.as_ref()?;
    let entry = resolve_entry_point_with_hint(&root, src, Some(target_name))?;
    Some(entry.to_string_lossy().into_owned())
}

/// Resolve the default entry point when no target is specified.
/// For single-project apps, finds the .fai file in the source dir.
/// For multi-target projects, errors (must specify a target).
pub(crate) fn resolve_default_entry_point() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let root = find_project_root(&cwd)?;
    resolve_default_entry_point_at(&root)
}

/// Pure variant of `resolve_default_entry_point` that takes the project
/// root explicitly. Extracted so tests can exercise the multi-target
/// error path without `std::env::set_current_dir` (which is a racy
/// operation under Rust's default multi-threaded test runner).
pub(crate) fn resolve_default_entry_point_at(root: &std::path::Path) -> Option<String> {
    let toml = std::fs::read_to_string(root.join("fai.toml")).ok()?;
    let info = parse_project_info(&toml);

    if !info.sub_projects.is_empty() {
        // Multi-target: if only one target, use it; otherwise require
        // -p / --project so the user picks a specific one. "fai run"
        // in a fullstack project has no sensible default — the server
        // and the client do different things.
        if info.sub_projects.len() == 1 {
            let (_, sub) = info.sub_projects.iter().next()?;
            let src = sub.source.as_ref()?;
            let entry = resolve_entry_point(root, src)?;
            return Some(entry.to_string_lossy().into_owned());
        }
        let n = info.sub_projects.len();
        eprintln!("error: --project required — this project has {} targets", n);
        eprintln!("usage:");
        let mut names: Vec<&String> = info.sub_projects.keys().collect();
        names.sort();
        for name in &names {
            eprintln!("  fai run --project {}", name);
        }
        return None;
    }

    // Single project: look for source = "src" or source_root convention
    let src = "src";
    let entry = resolve_entry_point(root, src)?;
    Some(entry.to_string_lossy().into_owned())
}

/// Iterate workspace members and run `cmd_build` on each one's entry
/// point. Members are relative directory paths from the workspace
/// root. Entry point resolution is convention-based for now (plan 99
/// Phase 2.2): `src/main.fai` if present, otherwise
/// `src/<name_lower>.fai`. A future `[[bin]]` table will let packages
/// declare their own entry explicitly.
pub(crate) fn cmd_build_workspace(root: &std::path::Path, members: &[String]) {
    eprintln!("building workspace with {} members", members.len());
    for m in members {
        let member_dir = root.join(m);
        if !member_dir.is_dir() {
            eprintln!(
                "  warning: workspace member '{}' — directory not found at {}",
                m,
                member_dir.display()
            );
            continue;
        }
        let info = read_project_info_full(Some(member_dir.to_str().unwrap()));
        let src_dir = member_dir.join("src");
        let main_candidate = src_dir.join("main.fai");
        let named_candidate = src_dir.join(format!("{}.fai", info.name.to_lowercase()));
        let entry = if main_candidate.is_file() {
            main_candidate
        } else if named_candidate.is_file() {
            named_candidate
        } else {
            eprintln!(
                "  warning: workspace member '{}' — no entry point found at {} or {}",
                m,
                main_candidate.display(),
                named_candidate.display()
            );
            continue;
        };
        eprintln!("\n▶ building member '{}' ({})", m, entry.display());
        cmd_build(&[entry.to_string_lossy().into_owned()]);
    }
}
