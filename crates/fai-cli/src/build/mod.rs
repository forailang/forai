use crate::*;

pub(crate) mod entry;
pub(crate) mod project;

pub(crate) use entry::*;
pub(crate) use project::*;

pub(crate) fn step_build(args: &[String], project: Option<&str>, reporter: &Reporter) {
    // Check for target name or file path
    let positional: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--") && !matches!(a.as_str(), "-o"))
        .map(|a| a.as_str())
        .collect();

    // Handle: `fai build` (no args) — build all targets
    // Handle: `fai build client` — build named target (lifted to `project` in cmd_build)
    // Handle: `fai build file.fai` — build specific file (backwards compat)
    if positional.is_empty() {
        // No args — try to build all targets from fai.toml
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Some(root) = find_project_root(&cwd) {
            let toml = std::fs::read_to_string(root.join("fai.toml")).unwrap_or_default();
            let info = parse_project_info(&toml);
            // New multi-target mode
            if !info.sub_projects.is_empty() {
                // Validate the named target exists before planning;
                // an unknown name should still print the targets list.
                if let Some(name) = project {
                    if !info.sub_projects.contains_key(name) {
                        eprintln!("error: unknown target '{}'. Available targets:", name);
                        for k in info.sub_projects.keys() {
                            eprintln!("  - {}", k);
                        }
                        std::process::exit(1);
                    }
                }
                let order = match plan_build_order(&info, project) {
                    Ok(o) => o,
                    Err(msg) => {
                        eprintln!("error: {}", msg);
                        std::process::exit(1);
                    }
                };
                for name in &order {
                    if let Some(sub) = info.sub_projects.get(name) {
                        build_one_subproject(name, sub, &root, &info);
                    }
                }
                return;
            }
            // Old workspace mode (backwards compat)
            if !info.workspace_members.is_empty() {
                cmd_build_workspace(&root, &info.workspace_members);
                return;
            }
        }
    }

    let first_arg = positional.first().copied().unwrap_or("");
    // Detect if the arg is a file path (has extension or path separator) vs a target name
    let is_file_path =
        first_arg.contains('.') || first_arg.contains('/') || first_arg.contains('\\');
    let path = if !first_arg.is_empty() && !is_file_path {
        // Target name — resolve from fai.toml and apply sub-project config
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Some(root) = find_project_root(&cwd) {
            let toml = std::fs::read_to_string(root.join("fai.toml")).unwrap_or_default();
            let ws_info = parse_project_info(&toml);
            if ws_info.sub_projects.contains_key(first_arg) {
                // Plan the dep order so any `required_targets` build
                // before the requested one. Asset copy happens per
                // target inside `build_one_subproject`.
                let order = match plan_build_order(&ws_info, Some(first_arg)) {
                    Ok(o) => o,
                    Err(msg) => {
                        eprintln!("error: {}", msg);
                        std::process::exit(1);
                    }
                };
                for name in &order {
                    if let Some(sub) = ws_info.sub_projects.get(name) {
                        build_one_subproject(name, sub, &root, &ws_info);
                    }
                }
                return;
            }
        }
        resolve_target_entry_point(first_arg).unwrap_or_else(|| {
            eprintln!("error: could not resolve target '{}'", first_arg);
            std::process::exit(1);
        })
    } else if !first_arg.is_empty() {
        first_arg.to_string()
    } else {
        // Single project with no sub-projects — find entry point
        resolve_default_entry_point().unwrap_or_else(|| {
            eprintln!("error: no file specified and no fai.toml found");
            std::process::exit(1);
        })
    };
    let mut content = read_file(&path);
    let source_root = find_source_root(&path);
    let info = read_project_info_full(source_root.as_deref());
    let build_dir_opt = info.build_dir.clone();

    inject_peer_hash(&mut content, &info, source_root.as_deref(), is_verbose());

    let build_native = matches!(info.target, Some(BuildTarget::Native));

    // Plan 101: Generate RPC dispatch (server) and proxy modules (client).
    inject_rpc_dispatch(&mut content, &info, source_root.as_deref(), Some(&path));

    let synthetic_modules = if !build_native {
        generate_rpc_proxy_modules(source_root.as_deref())
    } else {
        Vec::new()
    };

    // CLI --html flag wins when present; otherwise consult the toml
    // target. This keeps the old CLI flag working while letting new
    // projects declare it in toml.
    let generate_html =
        args.iter().any(|a| a == "--html") || matches!(info.target, Some(BuildTarget::WasmHtml));

    // Pass target to codegen so it can exclude unavailable host imports
    // (e.g. http_server_* for browser WASM targets).
    let codegen_target = if generate_html {
        Some("wasm-html")
    } else {
        None
    };
    if std::env::var_os("FAI_CHECK_LEAKS").is_some() {
        fai_codegen_wasm::set_check_leaks(true);
    }
    if std::env::var_os("FAI_OWNERSHIP_CHECK").is_some() {
        fai_codegen_wasm::set_ownership_check(true);
        fai_codegen_wasm::set_check_leaks(true);
    }
    if std::env::var_os("FAI_DEBUG_FUNCTION_CALLS").is_some() {
        fai_codegen_wasm::set_debug_function_calls(true);
    }

    // Find which sub-project's `main` matches this entry path so we
    // can read its `rpc_server` flag and remote-dependency URL. When
    // `rpc_server = false` (the default for client targets), every
    // `remote def` body is rewritten to call `remoteCall(...)` so the
    // client wasm never executes server-only code (the OOB on signup
    // in the browser was caused by the unrewritten `auth.signup`
    // dereferencing null SQLite handles). When `rpc_server = true` —
    // or when no remote URL is configured — the rewrite is skipped
    // and bodies stay intact.
    let canonical_entry = std::fs::canonicalize(&path).ok();
    let project_root_for_entry = canonical_entry
        .as_ref()
        .and_then(|entry| find_project_root(entry));
    let active_sub = {
        let canonical_entry = canonical_entry.clone();
        let project_root = project_root_for_entry.clone();
        info.sub_projects.iter().find(|(_, sub)| {
            sub.main
                .as_ref()
                .and_then(|m| {
                    let candidate = project_root
                        .as_ref()
                        .map(|root| root.join(m))
                        .unwrap_or_else(|| std::path::PathBuf::from(m));
                    std::fs::canonicalize(&candidate).ok()
                })
                .zip(canonical_entry.clone())
                .map(|(sub_main, entry)| sub_main == entry)
                .unwrap_or(false)
        })
    };
    let project_root_for_hash = project_root_for_entry.or_else(|| {
        source_root
            .as_deref()
            .and_then(|sr| find_project_root(std::path::Path::new(sr)))
    });
    if std::env::var_os("FAI_RPC_DEBUG").is_some() {
        eprintln!(
            "[rpc-proxy] entry={} source_root={:?} project_root={:?} active_target={:?} remote_deps={:?}",
            path,
            source_root,
            project_root_for_hash,
            active_sub.map(|(name, _)| name.as_str()),
            active_sub
                .map(|(_, sub)| sub.remote_deps.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        );
    }
    let rpc_proxy_substitution: Option<(String, String)> = match active_sub {
        Some((_, sub)) if !sub.rpc_server => sub.remote_deps.iter().find_map(|(dep_name, envs)| {
            let cfg = envs.get("dev").or_else(|| envs.values().next())?;
            let hash = project_root_for_hash
                .as_ref()
                .and_then(|root| find_dependency_hash(root, dep_name, &info))
                .unwrap_or_default();
            Some((cfg.url.clone(), hash))
        }),
        _ => None,
    };

    // Plan 94 Phase G: for default (non-html) builds try the direct
    // AST→wasm path before falling back to the bytecode codegen.
    // `wasm-html` forces bytecode because the direct module
    // assembler doesn't honour target-filtered imports yet.
    let mut wasm_bytes = compile_fai_to_wasm(
        &content,
        &path,
        false,
        synthetic_modules.clone(),
        codegen_target,
        rpc_proxy_substitution
            .as_ref()
            .map(|(u, h)| (u.as_str(), h.as_str())),
    );

    // Embed FFI extern metadata into the wasm so a prebuilt `.wasm`
    // dispatched via `fai run path/to/x.wasm` can rehydrate the
    // `call_ffi` table without re-reading the original source. No-op
    // when the project has no `extern` blocks (byte-identical output).
    let externs = extract_extern_info_full(&content, &path, synthetic_modules);
    wasm_runner::externs_section::embed_externs(&mut wasm_bytes, &externs);

    // Determine output directory and filename
    let output_path = if let Some(pos) = args.iter().position(|a| a == "-o") {
        args.get(pos + 1)
            .unwrap_or_else(|| {
                eprintln!("-o requires an output path");
                std::process::exit(1);
            })
            .clone()
    } else if generate_html {
        // Use build_dir from fai.toml (default: "public"), resolved relative to project root
        let build_dir = build_dir_opt.as_deref().unwrap_or("public");
        let project_root = source_root
            .as_deref()
            .and_then(|sr| std::path::Path::new(sr).parent())
            .unwrap_or_else(|| std::path::Path::new("."));
        let out_dir = project_root.join(build_dir);
        let _ = std::fs::create_dir_all(&out_dir);
        out_dir
            .join(artifact_filename(&info.name, &path))
            .to_str()
            .unwrap()
            .to_string()
    } else if let Some(bd) = build_dir_opt.as_deref() {
        // For non-html targets, honor [project].build_dir when set so a
        // hello-world starter declaring build_dir = "build" actually writes
        // there instead of dropping the wasm next to main.fai. When unset,
        // fall back to the historical "next to source" behavior.
        let project_root = source_root
            .as_deref()
            .and_then(|sr| std::path::Path::new(sr).parent())
            .unwrap_or_else(|| std::path::Path::new("."));
        let out_dir = project_root.join(bd);
        let _ = std::fs::create_dir_all(&out_dir);
        out_dir
            .join(artifact_filename(&info.name, &path))
            .to_str()
            .unwrap()
            .to_string()
    } else {
        // No build_dir: keep the wasm next to the source file. Filename
        // still derives from the project name when set.
        let dir = std::path::Path::new(&path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        dir.join(artifact_filename(&info.name, &path))
            .to_str()
            .unwrap()
            .to_string()
    };

    match std::fs::write(&output_path, &wasm_bytes) {
        Ok(_) => {
            reporter.detail(&format!(
                "compiled {} -> {} ({})",
                path,
                output_path,
                format_bytes(wasm_bytes.len()),
            ));
        }
        Err(e) => {
            reporter.error_line(&format!("error writing {}: {}", output_path, e));
            reporter.step(StepStatus::Fail, "build", "write error");
            std::process::exit(1);
        }
    }

    // Plan 116: extract the `fai-dbg` debug side-table (function index →
    // name/file/line) to `<out>.dbg.json` next to the wasm, so external
    // tools (browser harnesses, profilers) can map trap frames to source
    // without parsing the binary. The same data stays embedded in the
    // wasm for the native runner. Best-effort: a write failure is not a
    // build failure.
    if let Some(dbg_json) = extract_dbg_section(&wasm_bytes) {
        let dbg_path = format!("{}.dbg.json", output_path.trim_end_matches(".wasm"));
        let _ = std::fs::write(&dbg_path, dbg_json);
    }

    // Plan 101: If the target graph has remote functions/types, write
    // schema.json next to the build output so client builds can consume it.
    let mut wrote_schema = false;
    if let Ok(surface) =
        rpc_surface::collect_from_source(&content, source_root.as_deref(), Some(&path))
    {
        if !surface.is_empty() {
            let schema_dir = std::path::Path::new(&output_path)
                .parent()
                .unwrap_or(std::path::Path::new("."));
            let spec = surface.to_schema();
            let json = interface::spec_to_json(&spec);
            let schema_path = schema_dir.join("schema.json");
            if let Err(e) = std::fs::write(&schema_path, &json) {
                reporter.error_line(&format!("warning: could not write schema.json: {}", e));
            } else {
                wrote_schema = true;
                reporter.detail(&format!("generated {}", schema_path.display()));
            }
        }
    }

    // `target = "native"` → pack the wasm inside a copy of the
    // current forai binary. Produces a single-file deployable that
    // runs `_start` and exits. Plan 99 Phase 3.
    if build_native {
        let wasm_path = std::path::Path::new(&output_path);
        let out_dir = wasm_path.parent().unwrap_or(std::path::Path::new("."));
        let stem = wasm_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("app");
        let native_path = out_dir.join(stem);
        match pack_native_binary(&wasm_bytes, &native_path) {
            Ok(()) => reporter.detail(&format!(
                "packed native binary -> {} ({})",
                native_path.display(),
                format_bytes(
                    std::fs::metadata(&native_path)
                        .map(|m| m.len() as usize)
                        .unwrap_or(0)
                )
            )),
            Err(e) => {
                reporter.error_line(&format!("error packing native binary: {}", e));
                reporter.step(StepStatus::Fail, "build", "native pack error");
                std::process::exit(1);
            }
        }
    }

    // If --html flag, generate index.html + fai-runtime.js in the same directory
    if generate_html {
        let out_dir = std::path::Path::new(&output_path).parent().unwrap();
        let wasm_filename = std::path::Path::new(&output_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();

        // Write the runtime JS as a separate file
        let runtime_path = out_dir.join("fai-runtime.js");
        let runtime_js = generate_runtime_js(wasm_filename);
        match std::fs::write(&runtime_path, &runtime_js) {
            Ok(_) => reporter.detail(&format!("generated {}", runtime_path.display())),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                runtime_path.display(),
                e
            )),
        }

        // Write the default forui stylesheet next to the runtime.
        // User-facing modifier styles still win via inline style
        // attributes — this is just the component base look.
        let css_path = out_dir.join("forui.css");
        let css = generate_forui_css();
        match std::fs::write(&css_path, &css) {
            Ok(_) => reporter.detail(&format!("generated {}", css_path.display())),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                css_path.display(),
                e
            )),
        }

        // Write a minimal HTML file that loads the runtime
        let html_path = out_dir.join("index.html");
        let html = generate_html_page();
        match std::fs::write(&html_path, &html) {
            Ok(_) => reporter.detail(&format!(
                "generated {} (open in browser)",
                html_path.display()
            )),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                html_path.display(),
                e
            )),
        }
    }

    // If `[remote-interface] expose = true`, extract the package's
    // public interface spec and write it alongside the build output.
    // Peer packages pin against `interface.hash` so changes to the
    // shared surface surface as loud 401s rather than silent drift.
    // Plan 99 Phase 2.3.
    if info.interface_expose {
        let prepared = match fai_compiler::prepare_source(&content, source_root.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                reporter.error_line(&format!(
                    "warning: interface expose: prepare failed — {}",
                    e
                ));
                return;
            }
        };
        let spec =
            interface::extract_interface(&info.name, &info.version, &prepared.serde_ast.statements);
        let out_dir = std::path::Path::new(&output_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let json_path = out_dir.join("interface.json");
        let hash_path = out_dir.join("interface.hash");
        match std::fs::write(&json_path, interface::spec_to_json(&spec)) {
            Ok(_) => reporter.detail(&format!(
                "generated {} (hash: {})",
                json_path.display(),
                spec.hash
            )),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                json_path.display(),
                e
            )),
        }
        match std::fs::write(&hash_path, &spec.hash) {
            Ok(_) => reporter.detail(&format!("generated {}", hash_path.display())),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                hash_path.display(),
                e
            )),
        }
    }

    // Final step summary. Everything above emitted detail/warning
    // lines; the user-facing roll-up is one line per target.
    let mut summary_parts: Vec<String> = Vec::new();
    let wasm_stem = std::path::Path::new(&output_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&output_path);
    summary_parts.push(format!("{} {}", wasm_stem, format_bytes(wasm_bytes.len())));
    if wrote_schema {
        summary_parts.push("schema.json".to_string());
    }
    if generate_html {
        summary_parts.push("assets".to_string());
    }
    if build_native {
        summary_parts.push("native binary".to_string());
    }
    reporter.step(StepStatus::Ok, "build", &summary_parts.join(" + "));
}

/// Human-readable byte size for build-output summary lines.
/// `153224` → `"150 KB"`, `203957` → `"199 KB"`, `5_500_000` → `"5.2 MB"`.
/// Kept in the CLI crate (not a general util) because the only callers
/// are the build-step summary lines — we don't want to grow it into a
/// full size-formatter dependency.
pub(crate) fn format_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{} B", n)
    } else if n < 1024 * 1024 {
        format!("{} KB", n / 1024)
    } else {
        // One decimal place for MB so `5.2 MB` reads naturally.
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}
