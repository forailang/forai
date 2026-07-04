use super::*;

/// Plan 101: Generate RPC proxy code for remote dependencies.
/// Returns a list of synthetic modules (name, source) that should
/// be injected into the compiler. Any file can then
/// `use { getTasks } from Remote` to access the generated proxies.
pub(crate) fn generate_rpc_proxy_modules(source_root: Option<&str>) -> Vec<(String, String)> {
    let mut result = Vec::new();
    // Find the workspace root: walk up from source_root to find a
    // fai.toml that has sub-project definitions. We may pass through
    // per-project fai.toml files (old layout) before reaching the
    // workspace root.
    let project_root = match source_root {
        Some(sr) => {
            let mut dir = std::path::Path::new(sr).to_path_buf();
            let mut found = None;
            loop {
                let toml_path = dir.join("fai.toml");
                if toml_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&toml_path) {
                        let candidate = parse_project_info(&content);
                        if !candidate.sub_projects.is_empty() {
                            found = Some(dir.clone());
                            break;
                        }
                    }
                }
                if !dir.pop() {
                    break;
                }
            }
            match found {
                Some(root) => root,
                None => return result,
            }
        }
        None => return result,
    };
    let workspace_toml = project_root.join("fai.toml");
    let workspace_info = match std::fs::read_to_string(&workspace_toml) {
        Ok(content) => parse_project_info(&content),
        Err(_) => return result,
    };

    for (_sub_name, sub) in &workspace_info.sub_projects {
        for (dep_name, env_configs) in &sub.remote_deps {
            let url = match env_configs.get("dev") {
                Some(config) => &config.url,
                None => match env_configs.values().next() {
                    Some(config) => &config.url,
                    None => continue,
                },
            };

            let schema_json = find_server_schema(&project_root, dep_name, &workspace_info);
            let proxies = if let Some(schema) = schema_json {
                rpc_proxy::generate_proxies_from_schema(&schema, url).ok()
            } else {
                let source = find_dependency_source(&project_root, dep_name, &workspace_info);
                let hash = find_dependency_hash(&project_root, dep_name, &workspace_info)
                    .unwrap_or_default();
                source.and_then(|s| rpc_proxy::generate_proxies(&s, url, &hash).ok())
            };

            if let Some(proxies) = proxies {
                if !proxies.trim().is_empty() {
                    let module_name = capitalize_first(dep_name);
                    if is_verbose() {
                        eprintln!(
                            "    generated RPC proxies as '{}' module (url: {})",
                            module_name, url
                        );
                    }
                    result.push((module_name, proxies));
                }
            }
        }
    }
    result
}

/// Format a compiler-AST `TypeNode` back into source for round-tripping
/// when generating proxy bodies. Mirrors `rpc_proxy::format_type_node`
/// but for the compiler-AST (the parser-AST version is in rpc_proxy.rs).
fn format_compiler_type_node(tn: &fai_compiler::ast::TypeNode) -> String {
    let mut s = tn.name.clone().unwrap_or_else(|| "Void".to_string());
    if tn.is_array {
        s.push_str("[]");
    }
    if tn.is_optional {
        s.push('?');
    }
    s
}

/// Generate proxy fai source for a single `remote def`. The output is a
/// complete fai program containing one function with the same signature
/// as `fd` but a body that calls `remoteCall(url, key, args, hash)`. The
/// caller parses this, converts to compiler-AST, and lifts the body
/// statements over the original to swap in the proxy implementation.
fn generate_remote_def_proxy_source(
    fd: &fai_compiler::ast::FunctionDeclaration,
    key: &str,
    url: &str,
    hash: &str,
) -> String {
    let mut out = String::new();
    out.push_str("use std.json\n\n");
    out.push_str("# Auto-generated client proxy for a `remote def`.\n");
    out.push_str(&format!("def {}\n", fd.name));
    for p in &fd.params {
        out.push_str(&format!(
            "    @param {} {}\n",
            p.name,
            format_compiler_type_node(&p.type_node)
        ));
    }
    for r in &fd.return_types {
        out.push_str(&format!(
            "    @return {}\n",
            format_compiler_type_node(&r.type_node)
        ));
    }
    out.push_str("do\n");
    if fd.params.is_empty() {
        out.push_str(&format!(
            "  remoteCall('{}', '{}', '[]', '{}')\n",
            url, key, hash
        ));
    } else {
        let parts: Vec<String> = fd
            .params
            .iter()
            .map(|p| format!("json.stringify({})", p.name))
            .collect();
        out.push_str(&format!(
            "  let __args = '[' + {} + ']'\n",
            parts.join(" + ',' + ")
        ));
        out.push_str(&format!(
            "  remoteCall('{}', '{}', __args, '{}')\n",
            url, key, hash
        ));
    }
    out.push_str("end\n");
    out
}

/// Rewrite each `remote def` body in `modules` to call `remoteCall(...)`
/// instead of running the original (server-side) body. Triggered for
/// client targets with `rpc_server = false` (the default) so that
/// browser/wasm builds never execute server-only code paths like SQLite
/// access — the OOB seen on the signup flow was caused by `auth.signup`'s
/// real body running in the browser and dereferencing null Connection
/// handles.
///
/// Tests in the regular `fai test` flow keep the original bodies so unit
/// tests that exercise data-layer functions natively still work — this
/// rewrite is bypassed when `is_test` is true.
pub(crate) fn rewrite_remote_def_bodies(
    modules: &mut [fai_compiler::module::DiscoveredModule],
    url: &str,
    hash: &str,
) -> usize {
    let mut rewritten = 0usize;
    for module in modules.iter_mut() {
        let module_name = module.name.clone();
        let mut had_rewrite = false;
        for stmt in module.statements.iter_mut() {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = stmt {
                if !fd.is_remote {
                    continue;
                }
                let key = format!("{}.{}", module_name, fd.name);
                let proxy_src = generate_remote_def_proxy_source(fd, &key, url, hash);
                let parsed = match fai_parser::parse(&proxy_src) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("error: failed to parse rewritten proxy for {}: {}", key, e);
                        std::process::exit(1);
                    }
                };
                let serde = fai_compiler::native_bridge::convert_program(&parsed);
                let new_body = serde
                    .statements
                    .iter()
                    .find_map(|s| match s {
                        fai_compiler::ast::Statement::FunctionDeclaration(rfd)
                            if rfd.name == fd.name =>
                        {
                            Some(rfd.body.clone())
                        }
                        _ => None,
                    })
                    .expect("regenerated proxy must contain the named function");
                fd.body = new_body;
                // Mark non-remote so downstream code (RPC dispatch
                // generation, schema export) doesn't try to wire this
                // up server-side too — the body is now a client stub.
                // The @auth policy goes with it: enforcement lives at the
                // SERVER dispatch boundary; the client stub is a plain
                // def and the checker rejects @auth on non-remote fns.
                fd.is_remote = false;
                fd.auth_policy = None;
                had_rewrite = true;
                rewritten += 1;
            }
        }
        if had_rewrite {
            // The proxy body uses `json.stringify(...)` to serialise
            // arguments — make sure the module sees `std.json` even
            // when the original source didn't import it. Idempotent:
            // skip when the module already has `use std.json` (named
            // or namespace) at top level.
            let already_has_json = module.statements.iter().any(|s| {
                if let fai_compiler::ast::Statement::UseStatement(u) = s {
                    u.module_path == ["std".to_string(), "json".to_string()]
                } else {
                    false
                }
            });
            if !already_has_json {
                let zero = fai_compiler::ast::SourceLocation { line: 0, column: 0 };
                module.statements.insert(
                    0,
                    fai_compiler::ast::Statement::UseStatement(fai_compiler::ast::UseStatement {
                        module_path: vec!["std".to_string(), "json".to_string()],
                        imported_names: None,
                        import_all: false,
                        is_remote: false,
                        location: zero,
                    }),
                );
            }
        }
    }
    rewritten
}

/// Plan 101 Phase 4: Inject generated RPC dispatch for server targets.
/// The dispatch surface is every `remote def` reachable from the prepared
/// target graph, so endpoint modules can live in normal app folders.
pub(crate) fn inject_rpc_dispatch(
    content: &mut String,
    _info: &ProjectInfo,
    source_root: Option<&str>,
    entry_path: Option<&str>,
) {
    // Only inject dispatch if the source uses the RPC API.
    // Support both the new addRpcRoutes pattern and the legacy startRpcServer.
    let uses_rpc = content.contains("addRpcRoutes") || content.contains("startRpcServer");
    if !uses_rpc {
        return;
    }

    // Server targets expose every `remote def` reachable in their prepared
    // build graph. Endpoints can live in normal app modules (`data.tasks`,
    // `auth`, etc.); the generated dispatch imports them as needed.
    match rpc_surface::collect_from_source(content, source_root, entry_path) {
        Ok(surface) => {
            let dispatch_functions = surface.dispatch_functions();
            let dispatch =
                match rpc_dispatch::generate_dispatch_for_functions(&dispatch_functions, "") {
                    Ok(dispatch) => dispatch,
                    Err(e) => {
                        eprintln!("error: failed to generate addRpcRoutes: {}", e);
                        std::process::exit(1);
                    }
                };
            if !dispatch.trim().is_empty() {
                if is_verbose() {
                    eprintln!("    generated RPC dispatch (addRpcRoutes + handler + dispatch)");
                }
                content.push('\n');
                content.push_str(&dispatch);
            } else {
                // The server source calls addRpcRoutes/startRpcServer but has no
                // reachable 'remote def' functions for the dispatcher to route to.
                // Without this check, the build fails later with a cryptic
                // "unknown function 'addRpcRoutes'" — this tells agents exactly
                // what to add, and where.
                eprintln!(
                    "error: this server calls addRpcRoutes but no reachable 'remote def' functions were found"
                );
                eprintln!(
                    "       addRpcRoutes is auto-generated — and only generated when the server"
                );
                eprintln!("       target graph exposes at least one function. Mark each endpoint");
                eprintln!(
                    "       you want to expose as 'remote def' in any module imported by this target"
                );
                eprintln!("       (see `fai_examples rpc`).");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("  warning: failed to discover RPC surface: {}", e);
        }
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Find schema.json from a server sub-project's build dir.
fn find_server_schema(
    project_root: &std::path::Path,
    dep_name: &str,
    info: &ProjectInfo,
) -> Option<String> {
    let sub = info.sub_projects.get(dep_name)?;
    // Check build_dir first
    if let Some(bd) = &sub.build_dir {
        let schema = project_root.join(bd).join("schema.json");
        if let Ok(content) = std::fs::read_to_string(&schema) {
            return Some(content);
        }
    }
    // Check next to the main file
    if let Some(main) = &sub.main {
        let main_dir = project_root.join(main).parent()?.to_path_buf();
        let schema = main_dir.join("schema.json");
        if let Ok(content) = std::fs::read_to_string(&schema) {
            return Some(content);
        }
    }
    // Check source dir
    if let Some(src) = &sub.source {
        let schema = project_root.join(src).join("schema.json");
        if let Ok(content) = std::fs::read_to_string(&schema) {
            return Some(content);
        }
    }
    None
}

/// Find a sub-project dependency's source code by name.
///
/// Concatenates ALL .fai files from the server's source directory so that
/// `remote def` and `remote type` declarations spread across multiple files
/// are all visible to the proxy generator. Previously only the first file
/// found was read, causing silently incomplete proxies for multi-file servers.
fn find_dependency_source(
    project_root: &std::path::Path,
    dep_name: &str,
    info: &ProjectInfo,
) -> Option<String> {
    // Check if the dep is a sibling sub-project with a source path
    if let Some(sub) = info.sub_projects.get(dep_name) {
        if let Some(src_dir) = &sub.source {
            let src_path = project_root.join(src_dir);
            // If source/dep_name/ subdirectory exists, search there first
            // (handles source="src" with src/server/ layout)
            let search_dirs = vec![src_path.join(dep_name), src_path.clone()];
            for dir in &search_dirs {
                if !dir.is_dir() {
                    continue;
                }
                let mut files: Vec<_> = std::fs::read_dir(dir)
                    .ok()?
                    .flatten()
                    .filter(|e| {
                        e.path().extension().map_or(false, |x| x == "fai")
                            && !e
                                .path()
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .starts_with("test")
                    })
                    .collect();
                if files.is_empty() {
                    continue;
                }
                // Sort so main.fai comes first (its imports define the RPC surface).
                files.sort_by_key(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name == "main.fai" {
                        0u8
                    } else {
                        1u8
                    }
                });
                // Concatenate all files so remote def/type across files are all visible.
                let combined: String = files
                    .iter()
                    .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !combined.trim().is_empty() {
                    return Some(combined);
                }
            }
        }
    }

    // Fallback: look in conventional locations
    let fallback_paths = [
        project_root
            .join(dep_name)
            .join("src")
            .join(format!("{}.fai", dep_name)),
        project_root.join(dep_name).join("src").join("main.fai"),
    ];
    for p in &fallback_paths {
        if let Ok(content) = std::fs::read_to_string(p) {
            return Some(content);
        }
    }

    None
}

/// Find the interface hash for a dependency.
pub(crate) fn find_dependency_hash(
    project_root: &std::path::Path,
    dep_name: &str,
    info: &ProjectInfo,
) -> Option<String> {
    if let Some(sub) = info.sub_projects.get(dep_name) {
        if let Some(src_dir) = &sub.source {
            let hash_path = project_root.join(src_dir).join("interface.hash");
            if let Ok(h) = std::fs::read_to_string(&hash_path) {
                return Some(h.trim().to_string());
            }
        }
    }
    None
}

/// Find a project's fai.toml by walking up from the given directory
/// (typically cwd). Returns the directory containing the fai.toml.
pub(crate) fn find_project_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join("fai.toml").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}
