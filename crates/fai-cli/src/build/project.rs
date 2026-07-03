use crate::*;

/// Build target declared in `[project] target = "..."`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BuildTarget {
    /// Plain wasm output — the default. `forai run foo.wasm` loads it
    /// via wasmtime; servers typically run this.
    Wasm,
    /// Wasm plus the browser bundle (index.html, fai-runtime.js,
    /// forui.css). Equivalent to the historical `--html` CLI flag.
    WasmHtml,
    /// Native binary — bundles wasm + wasmtime into a single
    /// executable. Not implemented yet (plan 99 Phase 3); setting
    /// this in a fai.toml produces a build error rather than silent
    /// misbehaviour.
    Native,
}

impl BuildTarget {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "wasm" => Some(BuildTarget::Wasm),
            "wasm-html" => Some(BuildTarget::WasmHtml),
            "native" => Some(BuildTarget::Native),
            _ => None,
        }
    }
}

/// Per-environment remote service configuration.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RemoteEnvConfig {
    pub(crate) url: String,
}

/// A sub-project within a workspace (e.g. `[project.client]`).
#[derive(Debug, Default, Clone)]
pub(crate) struct SubProject {
    pub(crate) target: Option<BuildTarget>,
    pub(crate) source: Option<String>,
    /// Explicit entry point file (relative to project root).
    pub(crate) main: Option<String>,
    pub(crate) build_dir: Option<String>,
    /// Remote config for dependencies, keyed by dependency name then environment.
    pub(crate) remote_deps:
        std::collections::HashMap<String, std::collections::HashMap<String, RemoteEnvConfig>>,
    /// `true` when this sub-project hosts the RPC endpoint — `remote def`
    /// bodies stay intact and are wired into the dispatcher. When `false`
    /// (the default for client targets), each `remote def` reachable in
    /// this build has its body rewritten to `remoteCall(url, name, args,
    /// hash)` so the client never executes server-only code (e.g. SQLite
    /// access) under wasm. The URL comes from the matching
    /// `[project.X.dependencies.<dep>.remote.<env>]` entry.
    pub(crate) rpc_server: bool,
    /// `[project.<name>] required_targets = [...]` — names of other
    /// sub-projects whose builds must complete before this one. The
    /// build planner resolves these into a topological order and runs
    /// them first (cycle = build error).
    pub(crate) required_targets: Vec<String>,
    /// `[project.<name>.assets]` — ordered (from, to) pairs copied
    /// into this target's `build_dir` after a successful build.
    /// `from` starting with `$` references another target's
    /// `build_dir` (e.g. `$web` → that target's output directory);
    /// otherwise it is project-root-relative. `to` is relative to this
    /// target's `build_dir` (empty string = copy into the build_dir
    /// itself). Order is preserved so later entries overwrite earlier
    /// ones in the same destination.
    pub(crate) assets: Vec<(String, String)>,
}

/// One `[secrets]` declaration (plan 132):
/// `STRIPE_KEY = { required = true, targets = ["server"] }` or bare
/// `SLACK_TOKEN = {}` for an optional secret available to all targets.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SecretDecl {
    pub(crate) name: String,
    /// `required = true` — startup validation fails the boot when the
    /// active backend cannot resolve this name.
    pub(crate) required: bool,
    /// `targets = ["server"]` — restrict the declaration (and its
    /// startup validation) to the named sub-projects. Empty = all.
    pub(crate) targets: Vec<String>,
    /// `key = "field"` — pluck one field from a JSON secret value
    /// (aws backend).
    pub(crate) key: Option<String>,
}

/// The `[secrets]` manifest (plan 132): backend selection is config,
/// not code — the same program runs against `env` in dev and `aws` in
/// prod with no source change.
#[derive(Debug, Clone, Default)]
pub(crate) struct SecretsConfig {
    /// `backend = "env" | "dotenvx" | "aws"`. Defaults to `env`.
    pub(crate) backend: String,
    /// `allow_undeclared = true` — let `secrets.get` accept names outside
    /// the manifest at RUNTIME (for user-configured secret names that
    /// cannot be known statically, e.g. per-webhook signing secrets).
    /// The check-time rule is unaffected: literal names in source must
    /// still be declared.
    pub(crate) allow_undeclared: bool,
    pub(crate) declarations: Vec<SecretDecl>,
    /// Backend-specific config from `[secrets.<backend>]` sections,
    /// e.g. `[secrets.aws] region/prefix`, keyed by backend then key.
    pub(crate) backend_options:
        std::collections::HashMap<String, std::collections::HashMap<String, String>>,
}

impl SecretsConfig {
    /// Declarations that apply to the given active target name (a
    /// sub-project key, or None for a single-project/loose run).
    pub(crate) fn declarations_for_target(&self, target: Option<&str>) -> Vec<&SecretDecl> {
        self.declarations
            .iter()
            .filter(|d| {
                d.targets.is_empty()
                    || target.is_some_and(|t| d.targets.iter().any(|x| x == t))
            })
            .collect()
    }
}

/// Everything `forai build` reads out of the project's `fai.toml` up
/// front. Workspace + remote-interface information is also picked up
/// here so the build path only touches the file once.
#[derive(Debug, Default, Clone)]
pub(crate) struct ProjectInfo {
    /// `[project].name`. Defaults to `"unknown"` when absent.
    pub(crate) name: String,
    /// `[project].version`. Defaults to `"0.0.0"` when absent.
    pub(crate) version: String,
    /// `[project].build_dir`. `None` falls back to `"public"` under
    /// `wasm-html` builds.
    pub(crate) build_dir: Option<String>,
    /// `[project].target`. `None` means `"wasm"`.
    pub(crate) target: Option<BuildTarget>,
    /// `[workspace].members` — relative paths to member package
    /// directories. Non-empty means this fai.toml IS a workspace root
    /// rather than a package. In that case the `[project]` section is
    /// typically empty.
    pub(crate) workspace_members: Vec<String>,
    /// `[remote-interface].expose = true`. Build pipeline writes an
    /// `interface.json` + `interface.hash` alongside the wasm so peer
    /// packages can pin against it.
    pub(crate) interface_expose: bool,
    /// `[remote-interface].from = "..."`. Consumer packages read
    /// their peer's exposed interface.hash at build time and bake it
    /// into a generated `apiHash()` constant.
    pub(crate) interface_from: Option<String>,
    /// Named sub-projects (e.g. `[project.client]`, `[project.server]`).
    pub(crate) sub_projects: std::collections::HashMap<String, SubProject>,
    /// `[secrets]` manifest (plan 132). `None` when the section is absent.
    pub(crate) secrets: Option<SecretsConfig>,
}

/// Plan 132: every declared secret name (across all targets) for the
/// project owning `path` — the check-time declaration set for
/// `secrets.get` literal-name validation. `None` = no `[secrets]`
/// section (loose files stay unrestricted).
pub(crate) fn declared_secret_names_for_path(
    path: &str,
) -> Option<std::collections::HashSet<String>> {
    let source_root = find_source_root(path);
    read_project_info_full(source_root.as_deref())
        .secrets
        .map(|s| s.declarations.iter().map(|d| d.name.clone()).collect())
}

pub(crate) fn read_project_info(source_root: Option<&str>) -> (String, String, Option<String>) {
    let info = read_project_info_full(source_root);
    (info.name, info.version, info.build_dir)
}

/// Parse a fai.toml content string into a ProjectInfo. Extracted from
/// `read_project_info_full` for testability.
/// Pick the `.wasm` artifact filename for a build. The project's
/// `name` field wins; when it's the parser's default placeholder
/// (`"unknown"`) or empty, we fall back to the source file's stem so
/// ad-hoc `forai build foo.fai` runs against a loose file still produce
/// `foo.wasm`. The returned string includes the `.wasm` extension.
pub(crate) fn artifact_filename(project_name: &str, source_path: &str) -> String {
    let stem = if !project_name.is_empty() && project_name != "unknown" {
        project_name.to_string()
    } else {
        std::path::Path::new(source_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("main")
            .to_string()
    };
    format!("{}.wasm", stem)
}

/// Compute the `-o <path>` value for a sub-project build. Used by
/// both `cmd_build` paths (build-all and `-p <name>`). The artifact
/// is always named after the sub-project key (`web.wasm`,
/// `server.wasm`) — that name has to be passed explicitly via `-o`
/// because the recursive `cmd_build` call will re-parse the fai.toml
/// and otherwise pick up the top-level `[project].name`, which would
/// collide across sub-projects. Creates the output directory as a
/// side effect.
pub(crate) fn sub_project_output_path(
    sub: &SubProject,
    root: &std::path::Path,
    entry: &std::path::Path,
    sub_name: &str,
) -> String {
    let out_dir = match &sub.build_dir {
        Some(bd) => root.join(bd),
        None => entry
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| root.to_path_buf()),
    };
    let _ = std::fs::create_dir_all(&out_dir);
    out_dir
        .join(format!("{}.wasm", sub_name))
        .to_string_lossy()
        .into_owned()
}

/// Resolve the on-disk path of the `.wasm` artifact a sub-project
/// build produces. Used by `cmd_run` in project mode to skip the
/// in-memory compile and execute the just-built artifact directly.
/// Returns `None` when the project has no sub-projects, the named
/// target doesn't exist, or the artifact hasn't been built yet.
pub(crate) fn resolve_target_wasm_artifact(project: Option<&str>) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let root = find_project_root(&cwd)?;
    let toml = std::fs::read_to_string(root.join("fai.toml")).ok()?;
    let info = parse_project_info(&toml);
    if info.sub_projects.is_empty() {
        return None;
    }
    let name = match project {
        Some(n) => n.to_string(),
        None => {
            // No explicit target — only resolve when there's exactly
            // one sub-project. Multi-target with no `--project` is
            // ambiguous; the existing resolver handles that error.
            if info.sub_projects.len() == 1 {
                info.sub_projects.keys().next().cloned()?
            } else {
                return None;
            }
        }
    };
    let sub = info.sub_projects.get(&name)?;
    let dir = target_build_dir(&name, sub, &root)?;
    let path = dir.join(format!("{}.wasm", name));
    if path.exists() {
        Some(path.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Build a single sub-project: resolve its entry point, dispatch
/// through `cmd_build` (which runs the per-target fmt/check/test
/// pipeline + codegen), then copy `[project.<name>.assets]` into the
/// target's `build_dir`. Returns `true` on success, `false` when no
/// entry point could be resolved (the per-target message is printed
/// to stderr; the caller decides whether to continue).
///
/// Used by both `step_build` branches (build-all and build-one) and
/// by `cmd_run`'s build-then-run path. Keeping the build invocation
/// in one place ensures asset copies happen everywhere a build does.
pub(crate) fn build_one_subproject(
    name: &str,
    sub: &SubProject,
    root: &std::path::Path,
    info: &ProjectInfo,
) -> bool {
    let entry_opt = sub
        .main
        .as_ref()
        .map(|m| root.join(m))
        .filter(|p| p.is_file())
        .or_else(|| {
            sub.source
                .as_ref()
                .and_then(|src| resolve_entry_point_with_hint(root, src, Some(name)))
        });
    let Some(entry) = entry_opt else {
        eprintln!("  warning: target '{}' — no entry point found", name);
        return false;
    };
    eprintln!("\n▶ building target '{}' ({})", name, entry.display());
    let mut build_args = vec![entry.to_string_lossy().into_owned()];
    if matches!(sub.target, Some(BuildTarget::WasmHtml)) {
        build_args.push("--html".to_string());
    }
    build_args.push("-o".to_string());
    build_args.push(sub_project_output_path(sub, root, &entry, name));
    cmd_build(&build_args);
    copy_target_assets(name, sub, info, root);
    true
}

/// Resolve the absolute on-disk directory a target writes its build
/// artifacts to. Mirrors the rule used by `sub_project_output_path`:
/// `build_dir` from fai.toml when set, otherwise the directory of the
/// resolved entry file. Returns `None` when no entry can be resolved.
pub(crate) fn target_build_dir(
    name: &str,
    sub: &SubProject,
    root: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if let Some(bd) = &sub.build_dir {
        return Some(root.join(bd));
    }
    let entry = sub
        .main
        .as_ref()
        .map(|m| root.join(m))
        .filter(|p| p.is_file())
        .or_else(|| {
            sub.source
                .as_ref()
                .and_then(|src| resolve_entry_point_with_hint(root, src, Some(name)))
        })?;
    entry.parent().map(|p| p.to_path_buf())
}

/// Plan the build order for a set of targets. When `requested` is
/// `Some(name)`, returns the transitive closure of `name` (including
/// `name` itself) in dependency-first topological order. When `None`,
/// returns every sub-project in topological order. Names that don't
/// exist in `sub_projects` are dropped from `required_targets`
/// references (with a warning) rather than failing the build — this
/// keeps the parser permissive about typos in non-essential deps.
/// Cycles produce `Err(message)`; the caller exits the build.
pub(crate) fn plan_build_order(info: &ProjectInfo, requested: Option<&str>) -> Result<Vec<String>, String> {
    use std::collections::{HashMap, HashSet};

    // Topological sort with cycle detection. `visiting` is the
    // current DFS stack; `visited` is the finished set. Output is
    // built in post-order so dependencies appear before dependents.
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();

    // Start set: either the named target or every sub-project. When
    // walking everything, sort the roots alphabetically for a stable
    // build order across runs. Sibling subtrees that don't depend on
    // each other otherwise build in declaration-hash order, which
    // would be flaky in tests.
    let roots: Vec<String> = match requested {
        Some(name) => vec![name.to_string()],
        None => {
            let mut names: Vec<String> = info.sub_projects.keys().cloned().collect();
            names.sort();
            names
        }
    };

    fn visit(
        name: &str,
        sub_projects: &HashMap<String, SubProject>,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        order: &mut Vec<String>,
        path: &mut Vec<String>,
    ) -> Result<(), String> {
        if visited.contains(name) {
            return Ok(());
        }
        if visiting.contains(name) {
            // Reconstruct the cycle for the error message starting
            // from where `name` first appears in `path`.
            let cycle_start = path.iter().position(|n| n == name).unwrap_or(0);
            let mut cycle: Vec<String> = path[cycle_start..].to_vec();
            cycle.push(name.to_string());
            return Err(format!("required_targets cycle: {}", cycle.join(" -> ")));
        }
        let Some(sub) = sub_projects.get(name) else {
            // The requested name has no sub-project; let downstream
            // build-resolution surface a clearer error than this
            // planner can. Drop silently.
            return Ok(());
        };
        visiting.insert(name.to_string());
        path.push(name.to_string());
        for dep in &sub.required_targets {
            if !sub_projects.contains_key(dep) {
                eprintln!(
                    "  warning: target '{}' lists required_target '{}' which is not declared in fai.toml — skipping",
                    name, dep
                );
                continue;
            }
            visit(dep, sub_projects, visiting, visited, order, path)?;
        }
        path.pop();
        visiting.remove(name);
        visited.insert(name.to_string());
        order.push(name.to_string());
        Ok(())
    }

    let mut path: Vec<String> = Vec::new();
    for root in &roots {
        visit(
            root,
            &info.sub_projects,
            &mut visiting,
            &mut visited,
            &mut order,
            &mut path,
        )?;
    }
    Ok(order)
}

/// Recursive directory copy that merges into existing destinations
/// rather than replacing them. Files at the same relative path
/// overwrite. Used by `copy_target_assets` to layer multiple `assets`
/// entries that target the same directory (e.g. a generated client
/// bundle plus a project's authored `public/`).
pub(crate) fn copy_dir_merge(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if !src.exists() {
        return Ok(());
    }
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_merge(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Copy every `[project.<name>.assets]` entry into the target's
/// `build_dir`. Sources beginning with `$` reference another target's
/// `build_dir`; everything else is project-root relative. Destinations
/// are relative to this target's `build_dir` (empty string = the
/// build_dir itself). Errors print to stderr but don't fail the build —
/// the build artifact is already on disk and a missing optional asset
/// shouldn't take it down.
pub(crate) fn copy_target_assets(name: &str, sub: &SubProject, info: &ProjectInfo, root: &std::path::Path) {
    if sub.assets.is_empty() {
        return;
    }
    let Some(target_dir) = target_build_dir(name, sub, root) else {
        eprintln!(
            "  warning: target '{}' has assets but no resolvable build_dir — skipping copy",
            name
        );
        return;
    };
    for (from, to) in &sub.assets {
        let src_path: std::path::PathBuf = if let Some(target_ref) = from.strip_prefix('$') {
            match info
                .sub_projects
                .get(target_ref)
                .and_then(|s| target_build_dir(target_ref, s, root))
            {
                Some(p) => p,
                None => {
                    eprintln!(
                        "  warning: assets source '{}' for target '{}' references unknown target — skipping",
                        from, name
                    );
                    continue;
                }
            }
        } else {
            root.join(from)
        };
        let dst_path = if to.is_empty() {
            target_dir.clone()
        } else {
            target_dir.join(to)
        };
        if !src_path.exists() {
            eprintln!(
                "  warning: assets source '{}' for target '{}' does not exist at {} — skipping",
                from,
                name,
                src_path.display()
            );
            continue;
        }
        if let Err(e) = copy_dir_merge(&src_path, &dst_path) {
            eprintln!(
                "  warning: copying assets '{}' -> '{}' for target '{}' failed: {}",
                from, to, name, e
            );
        } else {
            eprintln!(
                "  copied assets {} -> {}",
                from,
                dst_path.strip_prefix(root).unwrap_or(&dst_path).display()
            );
        }
    }
}

/// Parse a single-line TOML string-array literal like `["a", "b"]`.
/// Returns an empty vec for any input that doesn't open with `[` and
/// close with `]`. Tolerant of whitespace and trailing commas.
/// Multi-line arrays are not supported by this hand-rolled TOML pass —
/// keep `required_targets` to one line.
pub(crate) fn parse_string_array(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = match trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(s) => s,
        None => return Vec::new(),
    };
    inner
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a TOML inline table (`{ required = true, targets = ["a", "b"] }`)
/// into (key, raw value) pairs. Commas inside `[...]` arrays don't split.
/// An empty table (`{}`) or a non-table value yields no pairs.
fn parse_inline_table(v: &str) -> Vec<(String, String)> {
    let v = v.trim();
    let Some(inner) = v.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut parts: Vec<String> = Vec::new();
    for c in inner.chars() {
        match c {
            '[' | '{' => {
                depth += 1;
                current.push(c);
            }
            ']' | '}' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    parts.push(current);
    for part in parts {
        if let Some((k, val)) = part.split_once('=') {
            let k = k.trim();
            if !k.is_empty() {
                out.push((k.to_string(), val.trim().to_string()));
            }
        }
    }
    out
}

pub(crate) fn parse_project_info(content: &str) -> ProjectInfo {
    let mut info = ProjectInfo {
        name: "unknown".into(),
        version: "0.0.0".into(),
        ..Default::default()
    };

    let mut section = String::new();
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix('[') {
            if let Some(name) = rest.strip_suffix(']') {
                section = name.trim().to_string();
            }
            continue;
        }
        let Some((k_raw, v_raw)) = t.split_once('=') else {
            continue;
        };
        let k = k_raw.trim();
        let v = v_raw.trim();
        let v_unquoted = v.trim_matches('"').to_string();

        // Check for sub-project sections: [project.client], [project.server], etc.
        if let Some(sub_name) = section.strip_prefix("project.") {
            // Could be [project.client] or [project.client.dependencies.shared.remote.dev]
            let parts: Vec<&str> = sub_name.split('.').collect();
            let sub_key = parts[0];
            let sub = info
                .sub_projects
                .entry(sub_key.to_string())
                .or_insert_with(SubProject::default);

            if parts.len() == 1 {
                // [project.client] — direct sub-project fields
                match k {
                    "target" => sub.target = BuildTarget::parse(&v_unquoted),
                    "source" => sub.source = Some(v_unquoted),
                    "main" => sub.main = Some(v_unquoted),
                    "build_dir" => sub.build_dir = Some(v_unquoted),
                    "rpc_server" => sub.rpc_server = v_unquoted == "true",
                    "required_targets" => {
                        sub.required_targets = parse_string_array(v);
                    }
                    _ => {}
                }
            } else if parts.len() == 2 && parts[1] == "assets" {
                // [project.client.assets] — ordered "from" = "to" pairs.
                // The key may be quoted (e.g. `"$web" = "public"`) so
                // strip surrounding quotes from both sides.
                let from = k.trim_matches('"').to_string();
                sub.assets.push((from, v_unquoted));
            } else if parts.len() >= 4 && parts[1] == "dependencies" && parts[3] == "remote" {
                // [project.client.dependencies.shared.remote.dev]
                let dep_name = parts[2];
                let env_name = parts.get(4).unwrap_or(&"dev");
                match k {
                    "url" => {
                        let env_map = sub
                            .remote_deps
                            .entry(dep_name.to_string())
                            .or_insert_with(std::collections::HashMap::new);
                        let config = env_map
                            .entry(env_name.to_string())
                            .or_insert_with(|| RemoteEnvConfig { url: String::new() });
                        config.url = v_unquoted;
                    }
                    _ => {}
                }
            }
            continue;
        }

        match section.as_str() {
            "project" => match k {
                "name" => info.name = v_unquoted,
                "version" => info.version = v_unquoted,
                "build_dir" => info.build_dir = Some(v_unquoted),
                "source" => { /* root-level source — for single-project */ }
                "target" => {
                    info.target = BuildTarget::parse(&v_unquoted);
                }
                _ => {}
            },
            "workspace" => {
                if k == "members" {
                    let inner = v.trim_start_matches('[').trim_end_matches(']');
                    for elem in inner.split(',') {
                        let m = elem.trim().trim_matches('"');
                        if !m.is_empty() {
                            info.workspace_members.push(m.to_string());
                        }
                    }
                }
            }
            "remote-interface" => match k {
                "expose" => info.interface_expose = v == "true",
                "from" => info.interface_from = Some(v_unquoted),
                _ => {}
            },
            "secrets" => {
                let secrets = info.secrets.get_or_insert_with(SecretsConfig::default);
                if k == "backend" {
                    secrets.backend = v_unquoted;
                } else if k == "allow_undeclared" {
                    secrets.allow_undeclared = v_unquoted == "true";
                } else {
                    // A declaration: `NAME = {}` or
                    // `NAME = { required = true, targets = ["server"] }`.
                    let mut decl = SecretDecl {
                        name: k.to_string(),
                        required: false,
                        targets: Vec::new(),
                        key: None,
                    };
                    for (ik, iv) in parse_inline_table(v) {
                        match ik.as_str() {
                            "required" => decl.required = iv == "true",
                            "targets" => decl.targets = parse_string_array(&iv),
                            "key" => decl.key = Some(iv.trim_matches('"').to_string()),
                            _ => {}
                        }
                    }
                    secrets.declarations.push(decl);
                }
            }
            s => {
                // `[secrets.<backend>]` — backend-specific options.
                if let Some(backend) = s.strip_prefix("secrets.") {
                    let secrets = info.secrets.get_or_insert_with(SecretsConfig::default);
                    secrets
                        .backend_options
                        .entry(backend.to_string())
                        .or_default()
                        .insert(k.to_string(), v_unquoted);
                }
            }
        }
    }

    if let Some(secrets) = info.secrets.as_mut() {
        if secrets.backend.is_empty() {
            secrets.backend = "env".to_string();
        }
    }

    info
}

/// Parses fai.toml from a source root directory. Delegates to
/// `parse_project_info` for the actual parsing.
pub(crate) fn read_project_info_full(source_root: Option<&str>) -> ProjectInfo {
    let Some(root) = source_root else {
        return ProjectInfo {
            name: "unknown".into(),
            version: "0.0.0".into(),
            ..Default::default()
        };
    };
    let src_path = std::path::Path::new(root);
    let toml_path = if src_path.join("fai.toml").exists() {
        src_path.join("fai.toml")
    } else if let Some(parent) = src_path.parent() {
        parent.join("fai.toml")
    } else {
        src_path.join("fai.toml")
    };
    let Ok(content) = std::fs::read_to_string(&toml_path) else {
        return ProjectInfo {
            name: "unknown".into(),
            version: "0.0.0".into(),
            ..Default::default()
        };
    };

    parse_project_info(&content)
}
