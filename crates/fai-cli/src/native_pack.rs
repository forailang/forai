use super::*;

pub(crate) fn cmd_interface(args: &[String]) {
    let path = require_file_arg(args, "interface");
    let content = read_file(&path);
    let source_root = find_source_root(&path);

    let prepared = match fai_compiler::prepare_source(&content, source_root.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // Read project name and version from fai.toml
    let (proj_name, proj_version, _) = read_project_info(source_root.as_deref());

    let spec =
        interface::extract_interface(&proj_name, &proj_version, &prepared.serde_ast.statements);

    let json = interface::spec_to_json(&spec);

    // Output to file or stdout
    if let Some(pos) = args.iter().position(|a| a == "-o") {
        if let Some(output_path) = args.get(pos + 1) {
            match std::fs::write(output_path, &json) {
                Ok(_) => eprintln!(
                    "interface spec written to {} (hash: {})",
                    output_path, spec.hash
                ),
                Err(e) => {
                    eprintln!("error writing {}: {}", output_path, e);
                    std::process::exit(1);
                }
            }
        } else {
            eprintln!("-o requires an output path");
            std::process::exit(1);
        }
    } else {
        println!("{}", json);
    }
}

/// Check `argv[0]` for an appended wasm + magic trailer. Returns the
/// wasm bytes if found, `None` otherwise.
///
/// Trailer layout (reading from end of file backwards):
///   bytes [N-8 .. N]   = wasm_len (u64 little-endian)
///   bytes [N-16 .. N-8] = NATIVE_TRAILER_MAGIC
///   bytes [N-16-wasm_len .. N-16] = wasm payload
pub(crate) fn read_embedded_wasm() -> Option<Vec<u8>> {
    let self_path = std::env::current_exe().ok()?;
    let bytes = std::fs::read(&self_path).ok()?;
    if bytes.len() < 16 {
        return None;
    }
    let n = bytes.len();
    let magic_start = n - 16;
    let len_start = n - 8;
    if &bytes[magic_start..len_start] != NATIVE_TRAILER_MAGIC {
        return None;
    }
    let len_bytes: [u8; 8] = bytes[len_start..n].try_into().ok()?;
    let wasm_len = u64::from_le_bytes(len_bytes) as usize;
    if wasm_len == 0 || wasm_len + 16 > n {
        return None;
    }
    let wasm_start = n - 16 - wasm_len;
    Some(bytes[wasm_start..magic_start].to_vec())
}

/// Produce a self-extracting native binary by copying the current
/// forai binary and appending the compiled wasm + a trailer. The
/// resulting file, when executed, loads its own tail and runs the
/// embedded wasm via wasmtime. Plan 99 Phase 3.2.
///
/// Returns Ok(path) on success. Errors bubble up from filesystem
/// operations; caller decides whether to warn or exit.
pub(crate) fn pack_native_binary(wasm_bytes: &[u8], output_path: &std::path::Path) -> Result<(), String> {
    // Test override: cargo test runs this code inside the test
    // binary, so `current_exe()` returns the test harness, not the
    // forai binary. Tests set FORAI_SELF_BINARY to the real forai
    // path (usually target/debug/forai).
    let self_path = if let Ok(override_path) = std::env::var("FORAI_SELF_BINARY") {
        std::path::PathBuf::from(override_path)
    } else {
        std::env::current_exe().map_err(|e| format!("could not locate forai binary: {}", e))?
    };
    let forai_bytes = std::fs::read(&self_path).map_err(|e| {
        format!(
            "could not read forai binary at {}: {}",
            self_path.display(),
            e
        )
    })?;
    let wasm_len = wasm_bytes.len() as u64;
    let mut out = Vec::with_capacity(forai_bytes.len() + wasm_bytes.len() + 16);
    out.extend_from_slice(&forai_bytes);
    out.extend_from_slice(wasm_bytes);
    out.extend_from_slice(NATIVE_TRAILER_MAGIC);
    out.extend_from_slice(&wasm_len.to_le_bytes());
    std::fs::write(output_path, out)
        .map_err(|e| format!("could not write {}: {}", output_path.display(), e))?;
    // chmod +x on Unix — the copied forai binary already has x bits
    // but some filesystems may strip them, and the permissions from
    // a fresh write() don't always mirror the source file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(output_path)
            .map_err(|e| format!("chmod: read metadata failed: {}", e))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(output_path, perms)
            .map_err(|e| format!("chmod: set_permissions failed: {}", e))?;
    }
    Ok(())
}

/// `[remote-interface] from = "PeerName"` → locate the peer's
/// interface.hash file (written by its own build with
/// `expose = true`) and inject a `peerHash()` function into the
/// consumer's source so user code can reference the live hash
/// instead of hand-writing a constant. Plan 99 Phase 2.4.
///
/// Applied by both `cmd_build` (when compiling to wasm) and `cmd_run`
/// (when invoking through the bytecode VM) so the injection is
/// invisible to the user regardless of execution path.
pub(crate) fn inject_peer_hash(
    content: &mut String,
    info: &ProjectInfo,
    source_root: Option<&str>,
    verbose: bool,
) {
    let Some(peer_name) = info.interface_from.as_deref() else {
        return;
    };
    match locate_peer_interface_hash(source_root, peer_name) {
        Some(hash) => {
            let stub = format!(
                "\n# Auto-generated by forai build/run from [remote-interface] from = \"{peer}\".\n\
                 # Returns the peer package's current interface hash.\n\
                 def peerHash\n    @return String\ndo\n  '{hash}'\nend\n",
                peer = peer_name,
                hash = hash
            );
            content.push_str(&stub);
            if verbose {
                eprintln!("  injected peerHash() = \"{}\" (from {})", hash, peer_name);
            }
        }
        None => {
            if verbose {
                eprintln!(
                    "warning: [remote-interface] from = \"{}\" — peer's interface.hash not found. \
                     Did you build the peer with [remote-interface] expose = true?",
                    peer_name
                );
            }
        }
    }
}

/// Find the `interface.hash` file produced by a peer package whose
/// `[project] name = "<peer_name>"` matches. Walks the consumer's
/// fai.toml `[dependencies]` to find the peer's project root, then
/// reads the hash from the conventional build output location
/// (currently `<peer_root>/src/interface.hash` — the default output
/// directory when no build_dir is set).
fn locate_peer_interface_hash(
    consumer_source_root: Option<&str>,
    peer_name: &str,
) -> Option<String> {
    let consumer_root = consumer_source_root.and_then(|sr| std::path::Path::new(sr).parent())?;
    let toml_path = consumer_root.join("fai.toml");
    let toml_content = std::fs::read_to_string(&toml_path).ok()?;

    let mut in_deps = false;
    for line in toml_content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            continue;
        }
        if !in_deps {
            continue;
        }
        // `Name = "file://path"` or `Name = "https://..."` — peer name
        // is on the LHS; skip entries whose LHS doesn't match the peer
        // we're looking up.
        let Some(spec) = fai_compiler::dep_url::parse_dep_line(t) else {
            continue;
        };
        if spec.name != peer_name {
            continue;
        }
        let Ok(dep_root_buf) = fai_compiler::dep_url::resolve_dep_url(&spec.url, consumer_root)
        else {
            continue;
        };
        let path_str = dep_root_buf.to_string_lossy().into_owned();
        let path_str = path_str.as_str();
        let dep_root = dep_root_buf.as_path();

        // Confirm the dep's own [project] name matches before trusting it.
        let dep_info =
            read_project_info_full(Some(dep_root.join("src").to_str().unwrap_or(path_str)));
        if dep_info.name != peer_name {
            continue;
        }

        // Peer matched — look for its interface.hash. Candidate
        // locations, in order: build_dir (if set), else src/.
        let build_dir = dep_info.build_dir.as_deref().unwrap_or("src");
        let hash_path = dep_root.join(build_dir).join("interface.hash");
        if let Ok(h) = std::fs::read_to_string(&hash_path) {
            return Some(h.trim().to_string());
        }
    }
    None
}
