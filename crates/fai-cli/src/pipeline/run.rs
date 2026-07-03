use crate::*;

pub(crate) fn step_run(args: &[String], project: Option<&str>, reporter: &Reporter) {
    // Phase D: `--wasm` is no longer a toggle — wasm is the only run
    // path. Accept the flag for back-compat with scripts that pass it;
    // the explicit `use_wasm` binding is kept as `_` so the filter at
    // the top of `positional` still skips it.
    let _use_wasm = args.iter().any(|a| a == "--wasm");

    // Plan 116 phase 2: `--watchdog[=secs]` / `--debug` arm the hang
    // watchdog — if the program hasn't completed after the deadline,
    // the runner interrupts it and prints a post-mortem dump (async
    // task table + heap stats). Equals-form only: positional-arg
    // detection above treats a bare value after `--watchdog` as a
    // target name. `FAI_WATCHDOG=<secs>` works as an env fallback.
    let watchdog_secs = args
        .iter()
        .find_map(|a| {
            if a == "--watchdog" || a == "--debug" {
                Some(10)
            } else {
                a.strip_prefix("--watchdog=").and_then(|v| v.parse().ok())
            }
        })
        .or(wasm_runner::RunOptions::from_env().watchdog_secs);
    // Plan 116 phase 5: `--check-leaks[=interval:<ms>]` arms the heap
    // allocation ledger. The flag has a codegen half (rt_alloc/rt_free
    // emit `__fai_alloc_event`/`__fai_free_event`) and a runner half
    // (record events, print the itemized live set at exit/trap, or on
    // an interval for servers). `FAI_CHECK_LEAKS=1|interval:<ms>` is
    // the env fallback.
    let check_leaks = args
        .iter()
        .find_map(|a| {
            if a == "--check-leaks" {
                Some(wasm_runner::CheckLeaksOptions::default())
            } else {
                a.strip_prefix("--check-leaks=")
                    .map(|v| wasm_runner::CheckLeaksOptions {
                        interval_ms: v.strip_prefix("interval:").and_then(|n| n.parse().ok()),
                    })
            }
        })
        .or(wasm_runner::RunOptions::from_env().check_leaks);
    if check_leaks.is_some() {
        // Codegen gate — must be set before compile_fai_to_wasm below.
        fai_codegen_wasm::set_check_leaks(true);
    }
    let check_ownership = args.iter().any(|a| a == "--check-ownership")
        || wasm_runner::RunOptions::from_env().check_ownership;
    if check_ownership {
        fai_codegen_wasm::set_ownership_check(true);
        fai_codegen_wasm::set_check_leaks(true);
    }
    let run_opts = wasm_runner::RunOptions {
        watchdog_secs,
        check_leaks,
        check_ownership,
    };

    // Check if the first positional arg is a target name or a file path.
    // If no positional arg, try project-based resolution from cwd.
    let positional: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--") && *a != "--wasm")
        .map(|a| a.as_str())
        .collect();

    // The active sub-project key (when known) — the `[secrets]`
    // per-declaration `targets = [...]` filter compares against it.
    let mut active_target: Option<String> = None;
    let path = if let Some(target) = project {
        // Explicit `--project NAME` — honour it over everything else.
        active_target = Some(target.to_string());
        resolve_target_entry_point(target).unwrap_or_else(|| {
            reporter.error_line(&format!("could not resolve target '{}'", target));
            reporter.step(StepStatus::Fail, "run", "unknown target");
            std::process::exit(1);
        })
    } else if let Some(arg) = positional.first() {
        let is_file = arg.contains('.') || arg.contains('/') || arg.contains('\\');
        if is_file {
            // Explicit file path — use it directly
            arg.to_string()
        } else {
            // Target name (legacy positional form, still supported)
            active_target = Some(arg.to_string());
            resolve_target_entry_point(arg).unwrap_or_else(|| {
                reporter.error_line(&format!("could not resolve target '{}'", arg));
                reporter.step(StepStatus::Fail, "run", "unknown target");
                std::process::exit(1);
            })
        }
    } else {
        // No arg — find the default/only target from fai.toml. When
        // the project has multiple sub-projects this prints a clear
        // `--project required` message and exits.
        resolve_default_entry_point().unwrap_or_else(|| {
            reporter.step(StepStatus::Fail, "run", "target not specified");
            std::process::exit(1);
        })
    };

    // Run pre-compiled .wasm files directly via Wasmtime
    if path.ends_with(".wasm") {
        let wasm_bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("error reading {}: {}", path, e);
                std::process::exit(1);
            }
        };
        if let Err(e) = wasm_runner::run_wasm_opts(&wasm_bytes, run_opts) {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return;
    }

    let mut content = read_file(&path);

    // Apply peerHash injection when `[remote-interface] from = "..."`
    // is set — the VM/JIT paths must see the same source the build
    // path does, or `peerHash()` won't resolve when running via
    // `forai run`. Plan 99 Phase 2.4.
    let run_source_root = find_source_root(&path);
    let run_info = read_project_info_full(run_source_root.as_deref());

    // Plan 132: startup secret validation. Fail-fast on missing required
    // secrets — with the name and backend, never a value — instead of
    // "crash at first use, maybe in production". The returned manifest is
    // installed on the runner below so `secrets_get` can reject
    // undeclared names at runtime.
    let secrets_manifest =
        match prepare_secrets(&run_info, active_target.as_deref(), run_source_root.as_deref()) {
            Ok(m) => m,
            Err(e) => {
                reporter.error_line(&e);
                reporter.step(StepStatus::Fail, "run", "missing required secrets");
                std::process::exit(1);
            }
        };

    inject_peer_hash(
        &mut content,
        &run_info,
        run_source_root.as_deref(),
        /* verbose = */ false,
    );

    // Plan 101: inject generated RPC dispatch/proxies for run path too.
    inject_rpc_dispatch(
        &mut content,
        &run_info,
        run_source_root.as_deref(),
        Some(&path),
    );

    // Generate synthetic RPC-proxy modules so `use { X } from Server`
    // resolves during the run-path's type check. Without this, a
    // fullstack server whose entry does `use { App } from client` (and
    // whose transitive client imports reach the `Server` proxy) fails
    // with `Unknown name 'App'` — the check step in compile_fai sees
    // the client's unresolved `Server` imports and cascades. `step_build`
    // and `step_check` (via `check_single_file`) already do this; run
    // now matches.
    let synthetic_modules = generate_rpc_proxy_modules(run_source_root.as_deref());

    // Phase H: the only input format is `.fai` source. The old
    // pre-compiled JSON-bytecode path lived on top of the bytecode→wasm
    // codegen — deleted along with `translate.rs` / `module.rs`.
    if !path.ends_with(".fai") {
        eprintln!(
            "error: only .fai source files are supported (pre-compiled JSON input was removed in Phase H)",
        );
        std::process::exit(1);
    }
    let wasm_bytes = compile_fai_to_wasm(
        &content,
        &path,
        false,
        synthetic_modules.clone(),
        None,
        None,
    );
    let externs = extract_extern_info_full(&content, &path, synthetic_modules);
    let _secrets_guard = wasm_runner::SecretsGuard::set(secrets_manifest);
    if let Err(e) = wasm_runner::run_wasm_with_externs_opts(&wasm_bytes, externs, run_opts) {
        reporter.error_line(&e);
        reporter.step(StepStatus::Fail, "run", "runtime error");
        std::process::exit(1);
    }
}

/// Plan 132: build the active [`wasm_runner::SecretsManifest`] from the
/// project's `[secrets]` section and fail-fast validate required secrets
/// for the active target.
///
/// The `env` backend host-loads the project's `.env` (beside fai.toml)
/// into the process environment first — real environment variables win
/// over file entries — so declared secrets in a local dotenv file resolve
/// both here and later at egress. Later backends (dotenvx phase 4, aws
/// phase 5) plug in behind the same manifest.
///
/// Errors name the secret and the backend, never a value.
pub(crate) fn prepare_secrets(
    info: &ProjectInfo,
    active_target: Option<&str>,
    source_root: Option<&str>,
) -> Result<Option<wasm_runner::SecretsManifest>, String> {
    let Some(cfg) = &info.secrets else {
        return Ok(None);
    };

    // Project dir = where fai.toml lives (mirror read_project_info_full's
    // discovery: source_root itself, else its parent).
    let project_dir = source_root.map(|root| {
        let root = std::path::Path::new(root);
        if root.join("fai.toml").exists() {
            root.to_path_buf()
        } else {
            root.parent().unwrap_or(root).to_path_buf()
        }
    });

    let decls = cfg.declarations_for_target(active_target);

    // Values pre-resolved host-side by the backend. These ride the
    // manifest into the runner and stay host-side. (The aws backend
    // resolves through its own TTL cache instead — see below.)
    let mut resolved: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    // A previous run in this process may have installed aws state.
    crate::aws_secrets::clear();

    match cfg.backend.as_str() {
        "env" | "" => {
            // Merge `.env` beside fai.toml into the process environment
            // (real env wins) so declared secrets in a local dotenv file
            // resolve both here and at egress.
            if let Some(dir) = &project_dir {
                if let Ok(content) = std::fs::read_to_string(dir.join(".env")) {
                    for (key, value) in wasm_runner::parse_dotenv(&content) {
                        if std::env::var_os(&key).is_none() {
                            // SAFETY: same single-threaded-before-run
                            // argument as `env.load` in host/env.rs —
                            // validation runs before the module (and any
                            // worker) starts.
                            #[allow(unused_unsafe)]
                            unsafe {
                                std::env::set_var(key, value);
                            }
                        }
                    }
                }
            }
        }
        "dotenvx" => {
            // Encrypted `.env` (plan 132 phase 4): values decrypt into
            // the HOST-SIDE resolved map only — nothing is merged into
            // the process environment, so child processes inherit
            // nothing and plaintext exposure stays at the egress points.
            let dir = project_dir
                .as_ref()
                .ok_or_else(|| "dotenvx backend: no project directory found".to_string())?;
            let env_path = dir.join(".env");
            let content = std::fs::read_to_string(&env_path).map_err(|_| {
                "dotenvx backend: missing .env beside fai.toml".to_string()
            })?;
            // Private key: real environment first, then `.env.keys`
            // (never committed) beside fai.toml. Comma-separated list
            // supports dotenvx key rotation.
            let private_keys = std::env::var("DOTENV_PRIVATE_KEY").ok().or_else(|| {
                std::fs::read_to_string(dir.join(".env.keys"))
                    .ok()
                    .and_then(|keys| {
                        wasm_runner::parse_dotenv(&keys)
                            .into_iter()
                            .find(|(k, _)| k == "DOTENV_PRIVATE_KEY")
                            .map(|(_, v)| v)
                    })
            });
            for (key, value) in wasm_runner::parse_dotenv(&content) {
                if key == "DOTENV_PUBLIC_KEY" {
                    continue;
                }
                if value.starts_with(crate::dotenvx::ENCRYPTED_PREFIX) {
                    let Some(keys) = &private_keys else {
                        return Err(format!(
                            "dotenvx backend: '{}' is encrypted but no \
                             DOTENV_PRIVATE_KEY is set (environment or .env.keys)",
                            key
                        ));
                    };
                    let plain =
                        crate::dotenvx::decrypt_value_multi(keys, &value).map_err(|e| {
                            format!("dotenvx backend: cannot decrypt '{}': {}", key, e)
                        })?;
                    resolved.insert(key, plain);
                } else {
                    // dotenvx keeps non-secret entries plaintext.
                    resolved.insert(key, value);
                }
            }
        }
        "aws" => {
            // AWS Secrets Manager (plan 132 phase 5): fetch every declared
            // secret at startup into the host-side TTL cache. Blocking is
            // fine here — the module hasn't started; after this, egress
            // resolution is cache-only (stale-while-revalidate keeps the
            // scheduler free of I/O).
            let opts = cfg.backend_options.get("aws");
            let region = opts
                .and_then(|o| o.get("region"))
                .cloned()
                .ok_or_else(|| "aws backend: [secrets.aws] region is required".to_string())?;
            let prefix = opts
                .and_then(|o| o.get("prefix"))
                .cloned()
                .unwrap_or_default();
            let endpoint = opts.and_then(|o| o.get("endpoint")).cloned();
            let ttl_secs = opts
                .and_then(|o| o.get("ttl"))
                .and_then(|t| t.parse().ok())
                .unwrap_or(crate::aws_secrets::DEFAULT_TTL_SECS);
            let credentials = crate::aws_secrets::AwsCredentials::from_env()?;
            let field_map = decls
                .iter()
                .filter_map(|d| d.key.as_ref().map(|k| (d.name.clone(), k.clone())))
                .collect();
            crate::aws_secrets::configure(
                crate::aws_secrets::AwsConfig {
                    region,
                    prefix,
                    endpoint,
                    ttl: std::time::Duration::from_secs(ttl_secs),
                    field_map,
                },
                credentials,
                decls.iter().map(|d| d.name.clone()).collect(),
            )?;
        }
        other => {
            return Err(format!(
                "unknown secrets backend '{}' (supported: env, dotenvx, aws)",
                other
            ));
        }
    }

    let missing: Vec<String> = decls
        .iter()
        .filter(|d| {
            d.required
                && !resolved.contains_key(&d.name)
                && crate::aws_secrets::resolve(&d.name).is_none()
                && std::env::var_os(&d.name).is_none()
        })
        .map(|d| format!("'{}'", d.name))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required secret{} {} (backend {})",
            if missing.len() > 1 { "s" } else { "" },
            missing.join(", "),
            cfg.backend,
        ));
    }

    Ok(Some(wasm_runner::SecretsManifest {
        backend: cfg.backend.clone(),
        allow_undeclared: cfg.allow_undeclared,
        declared: decls.iter().map(|d| d.name.clone()).collect(),
        resolved,
    }))
}

/// Build the host-side extern table by walking the entry file plus
/// every resolved dependency module — matching the codegen's
/// `extern_fn_indices` ordering (entry first, then modules in
/// discovery order). The wasm runner's `call_ffi` import indexes
/// into this table by `ext_fn_idx`. `[ffi.<name>].lib` in fai.toml
/// overrides the C library name; otherwise the block's own `extern
/// <name>` identifier is used.
pub(crate) fn extract_extern_info_full(
    content: &str,
    path: &str,
    synthetic_modules: Vec<(String, String)>,
) -> Vec<wasm_runner::ExternInfo> {
    let source_root = find_source_root(path);

    // Run the same compile pre-pass the codegen uses so we see the
    // identical set of modules (and in the same order). Extern blocks
    // live in the compiler-side AST; iterate entry.statements first,
    // then each module's statements.
    let prepared = match fai_compiler::prepare_source_with_synthetic_and_entry(
        content,
        source_root.as_deref(),
        synthetic_modules,
        Some(path),
    ) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    // Each module (or entry) may root a different `fai.toml` — load
    // the ffi config from the one the source root points at.
    let ffi_config = source_root
        .as_deref()
        .map(fai_compiler::ffi_config::load_ffi_config)
        .unwrap_or_default();
    // Library-name override: only from the entry project's own
    // fai.toml. Per-dependency `[ffi.*]` overrides would require the
    // compiler to expose each module's source root — DiscoveredModule
    // doesn't carry that today. In practice every fai dep so far uses
    // `extern <libname>` directly, so the override is optional.
    let resolve_lib = |block_name: &str| -> String {
        ffi_config
            .libraries
            .get(block_name)
            .map(|lc| lc.lib.clone())
            .unwrap_or_else(|| block_name.to_string())
    };

    let mut externs = Vec::new();
    let push_block = |block: &fai_compiler::ast::ExternBlockDeclaration,
                      externs: &mut Vec<wasm_runner::ExternInfo>| {
        let library = resolve_lib(&block.library);
        for decl in &block.functions {
            let param_types: Vec<wasm_runner::FfiType> = decl
                .params
                .iter()
                .map(|p| compiler_typenode_to_ffi_type(&p.type_node, p.is_out))
                .collect();
            let return_type = decl
                .return_type
                .as_ref()
                .map(|tn| compiler_typenode_to_ffi_type(tn, false))
                .unwrap_or(wasm_runner::FfiType::Void);
            externs.push(wasm_runner::ExternInfo {
                library: library.clone(),
                function: decl.name.clone(),
                param_types,
                return_type,
            });
        }
    };

    for stmt in &prepared.serde_ast.statements {
        if let fai_compiler::ast::Statement::ExternBlockDeclaration(block) = stmt {
            push_block(block, &mut externs);
        }
    }
    for module in &prepared.modules {
        for stmt in &module.statements {
            if let fai_compiler::ast::Statement::ExternBlockDeclaration(block) = stmt {
                push_block(block, &mut externs);
            }
        }
    }
    externs
}

/// Like `extract_extern_info_full` but takes an already-prepared
/// program. Used by the test-step paths that already have the
/// `PreparedProgram` on hand and don't want to re-parse.
pub(crate) fn extract_externs_from_prepared(
    prepared: &fai_compiler::PreparedProgram,
    source_root: Option<&str>,
) -> Vec<wasm_runner::ExternInfo> {
    let ffi_config = source_root
        .map(fai_compiler::ffi_config::load_ffi_config)
        .unwrap_or_default();
    let resolve_lib = |block_name: &str| -> String {
        ffi_config
            .libraries
            .get(block_name)
            .map(|lc| lc.lib.clone())
            .unwrap_or_else(|| block_name.to_string())
    };
    let mut externs = Vec::new();
    let push_block = |block: &fai_compiler::ast::ExternBlockDeclaration,
                      externs: &mut Vec<wasm_runner::ExternInfo>| {
        let library = resolve_lib(&block.library);
        for decl in &block.functions {
            let param_types: Vec<wasm_runner::FfiType> = decl
                .params
                .iter()
                .map(|p| compiler_typenode_to_ffi_type(&p.type_node, p.is_out))
                .collect();
            let return_type = decl
                .return_type
                .as_ref()
                .map(|tn| compiler_typenode_to_ffi_type(tn, false))
                .unwrap_or(wasm_runner::FfiType::Void);
            externs.push(wasm_runner::ExternInfo {
                library: library.clone(),
                function: decl.name.clone(),
                param_types,
                return_type,
            });
        }
    };
    for stmt in &prepared.serde_ast.statements {
        if let fai_compiler::ast::Statement::ExternBlockDeclaration(block) = stmt {
            push_block(block, &mut externs);
        }
    }
    for module in &prepared.modules {
        for stmt in &module.statements {
            if let fai_compiler::ast::Statement::ExternBlockDeclaration(block) = stmt {
                push_block(block, &mut externs);
            }
        }
    }
    externs
}

fn compiler_typenode_to_ffi_type(
    tn: &fai_compiler::ast::TypeNode,
    is_out: bool,
) -> wasm_runner::FfiType {
    use wasm_runner::FfiType;
    if is_out {
        return FfiType::OutPtr;
    }
    let name = tn.name.as_deref().unwrap_or("");
    match name {
        "Int" => FfiType::Int,
        "Float" => FfiType::Double,
        "String" => FfiType::String,
        "Bool" => FfiType::Bool,
        "Ptr" => FfiType::Pointer,
        "Void" => FfiType::Void,
        _ => FfiType::Pointer,
    }
}
