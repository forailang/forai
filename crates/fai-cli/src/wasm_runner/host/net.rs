//! Networking host imports: `http_post`, `http_request_*`, `remote_call`.

use wasmtime::*;

use super::super::heap::build_value;
#[cfg(feature = "http-client")]
use super::super::heap::wasm_alloc_str;
use super::super::nan_box::{ADDR_MASK, OBJ_TAG_DICT, QNAN, SIGN_BIT, VAL_NULL};
#[cfg(feature = "http-client")]
use super::events::{alloc_dict, write_global_i32, write_global_i64};
use super::host_ops::{
    read_string_value, signal_host_error, submit_host_op, submit_host_wait, HostOpResult,
};

#[cfg(feature = "http-client")]
const HTTP_CLIENT_TIMEOUT_SECS: u64 = 120;

/// Build a forai-shaped Error Dict (`{ message, kind }`) in guest
/// memory and stash it into `__error_value` with `__error_flag = 1`
/// so the post-call propagation in the wasm binary picks it up as if
/// the host had `throw`n. Returns `VAL_NULL` as the result-stack
/// placeholder — the caller's emit_post_call_propagation reads the
/// flag, sees it set, and skips the placeholder.
#[cfg(feature = "http-client")]
fn signal_remote_call_error(caller: &mut Caller<'_, ()>, message: &str) -> i64 {
    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return VAL_NULL,
    };
    let key_message = wasm_alloc_str(caller, &mem, "message");
    let key_kind = wasm_alloc_str(caller, &mem, "kind");
    let v_message = wasm_alloc_str(caller, &mem, message);
    let v_kind = wasm_alloc_str(caller, &mem, "remote");
    let err_box = alloc_dict(
        caller,
        &mem,
        &[(key_message, v_message), (key_kind, v_kind)],
    );
    write_global_i32(caller, "__error_flag", 1);
    write_global_i64(caller, "__error_value", err_box);
    VAL_NULL
}

/// Mirror of `fai-runtime::natives::discover_c_library` for parity.
/// Checks pkg-config, then common system paths, then Homebrew.
fn discover_c_library(lib_name: &str) -> bool {
    // pkg-config
    if let Ok(status) = std::process::Command::new("pkg-config")
        .args(["--exists", lib_name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        if status.success() {
            return true;
        }
    }
    // Common linker prefixes.
    for prefix in [
        "/usr/lib",
        "/usr/local/lib",
        "/lib",
        "/opt/homebrew/lib",
        "/usr/lib/x86_64-linux-gnu",
    ] {
        for ext in ["so", "dylib", "a"] {
            let candidate = format!("{}/lib{}.{}", prefix, lib_name, ext);
            if std::path::Path::new(&candidate).exists() {
                return true;
            }
        }
    }
    false
}

/// Read a UTF-8 slice from guest memory. Empty string on bounds error.
fn read_str(mem: &Memory, caller: &impl AsContext, ptr: i32, len: i32) -> String {
    let data = mem.data(caller);
    let start = ptr as usize;
    let end = start.saturating_add(len as usize);
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

/// Percent-encode a value as an `application/x-www-form-urlencoded` component:
/// every byte outside the RFC 3986 unreserved set becomes `%XX`.
fn percent_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Percent-decode; `+` decodes to a space (form-encoding convention). A
/// malformed `%` escape is passed through literally. Result is UTF-8 (lossy).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// True if an address must never be reached by the credential proxy: loopback,
/// private, link-local, unspecified, broadcast, CGNAT, or an IPv4-mapped v6 of
/// any of those. This is the SSRF fence (plan R15/R15a).
#[cfg(feature = "http-client")]
fn ip_is_blocked(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || v6
                    .to_ipv4_mapped()
                    .map(|m| ip_is_blocked(IpAddr::V4(m)))
                    .unwrap_or(false)
        }
    }
}

/// Extract (host, port) from an absolute URL without a URL-parsing crate.
/// Defaults the port by scheme. Returns None if the URL has no authority.
#[cfg(feature = "http-client")]
fn host_port_from_url(url: &str) -> Option<(String, u16)> {
    let is_https = url.starts_with("https");
    let default_port = if is_https { 443 } else { 80 };
    let after_scheme = url.split("://").nth(1)?;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    // Drop any userinfo prefix.
    let authority = authority.rsplit('@').next()?;
    if let Some(rest) = authority.strip_prefix('[') {
        // [ipv6]:port
        let (h, tail) = rest.split_once(']')?;
        let port = tail
            .strip_prefix(':')
            .and_then(|s| s.parse().ok())
            .unwrap_or(default_port);
        return Some((h.to_string(), port));
    }
    match authority.split_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok().unwrap_or(default_port))),
        None => Some((authority.to_string(), default_port)),
    }
}

/// Resolve the URL's host and refuse if it maps to any non-public address.
/// Resolution failure is treated as a refusal (fail closed). This runs per
/// guarded call; brain re-dispatches each same-host redirect hop as a fresh
/// guarded call, so every hop is re-checked. Residual: a sub-second DNS
/// rebind between this check and ureq's connect is not pinned out (v1 relies
/// on the operator-trusted deployment precondition; pin-to-resolved-IP is
/// deferred).
#[cfg(feature = "http-client")]
fn guard_host_public(url: &str) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    let Some((host, port)) = host_port_from_url(url) else {
        return Err("guarded: could not parse host from URL".to_string());
    };
    let addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("guarded: DNS resolution failed: {}", e))?;
    let mut any = false;
    for addr in addrs {
        any = true;
        if ip_is_blocked(addr.ip()) {
            return Err(format!("guarded: {} resolves to a blocked address", host));
        }
    }
    if !any {
        return Err(format!("guarded: {} did not resolve", host));
    }
    Ok(())
}

/// Hardened outbound request for the credential proxy. Applies the SSRF IP
/// guard, a no-follow-by-default redirect policy, and a response size cap with
/// a truncation marker. Returns a Response value (status/body/headers) or None
/// on refusal/transport failure.
#[cfg(feature = "http-client")]
fn do_guarded_request(
    method: &str,
    url: &str,
    body: Option<&str>,
    request_headers: &[(String, String)],
    options_json: &str,
) -> Option<serde_json::Value> {
    let opts: serde_json::Value = serde_json::from_str(options_json).unwrap_or(serde_json::json!({}));
    let block_private = opts
        .get("blockPrivateIps")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let follow = opts
        .get("followRedirects")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_redirects = opts.get("maxRedirects").and_then(|v| v.as_u64()).unwrap_or(3) as u32;
    let max_bytes = opts
        .get("maxBytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(1_048_576);

    if block_private && guard_host_public(url).is_err() {
        return None;
    }

    let redirects = if follow { max_redirects.max(1) } else { 0 };
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(redirects)
        .max_redirects_will_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(HTTP_CLIENT_TIMEOUT_SECS)))
        .build()
        .new_agent();

    let has_content_type = request_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));

    let req_result = match (method, body) {
        ("GET", _) => {
            let mut req = agent.get(url);
            for (n, v) in request_headers {
                req = req.header(n, v);
            }
            req.call()
        }
        ("DELETE", _) => {
            let mut req = agent.delete(url);
            for (n, v) in request_headers {
                req = req.header(n, v);
            }
            req.call()
        }
        (m, Some(b)) if m == "POST" || m == "PUT" || m == "PATCH" => {
            let mut req = match m {
                "POST" => agent.post(url),
                "PUT" => agent.put(url),
                _ => agent.patch(url),
            };
            if !has_content_type {
                req = req.header("Content-Type", "application/json");
            }
            for (n, v) in request_headers {
                req = req.header(n, v);
            }
            req.send(b.as_bytes())
        }
        _ => return None,
    };

    match req_result {
        Ok(resp) => {
            let status = resp.status().as_u16() as i32;
            let mut headers: Vec<(String, String)> = Vec::new();
            for (name, val) in resp.headers().iter() {
                if let Ok(v) = val.to_str() {
                    headers.push((name.as_str().to_string(), v.to_string()));
                }
            }
            let mut body = resp.into_body();
            let raw = body
                .with_config()
                .limit(max_bytes.saturating_add(1))
                .read_to_vec()
                .unwrap_or_default();
            let (bytes, truncated) = if raw.len() as u64 > max_bytes {
                (&raw[..max_bytes as usize], true)
            } else {
                (&raw[..], false)
            };
            let mut text = String::from_utf8_lossy(bytes).into_owned();
            if truncated {
                text.push_str(&format!("\n[truncated at {} bytes]", max_bytes));
            }
            Some(build_http_response_value(status, &text, &headers))
        }
        Err(_) => None,
    }
}

#[cfg(not(feature = "http-client"))]
fn do_guarded_request(
    _method: &str,
    _url: &str,
    _body: Option<&str>,
    _request_headers: &[(String, String)],
    _options_json: &str,
) -> Option<serde_json::Value> {
    None
}

#[cfg(feature = "http-client")]
fn build_http_response_value(
    status: i32,
    body: &str,
    headers: &[(String, String)],
) -> serde_json::Value {
    use serde_json::{json, Map, Value};
    let mut hdr_map = Map::new();
    for (k, v) in headers {
        hdr_map.insert(k.clone(), Value::String(v.clone()));
    }
    json!({
        "status": status,
        "body": body,
        "headers": Value::Object(hdr_map),
    })
}

/// file:// URL handling for VM parity. `native_http_get` / `native_http_post`
/// treat `file://path` as a local-file read/write and return a synthetic
/// 200 response. The host impls mirror that.
fn file_url_to_path(url: &str) -> Option<&str> {
    url.strip_prefix("file://")
}

/// Dispatch a single HTTP verb. Handles `file://` URLs for parity with
/// `native_http_*` in fai-runtime, then (feature-gated) issues a real
/// request via ureq. Returns a NaN-boxed Dict on success, `VAL_NULL` on
/// any transport failure.
fn do_verb(
    caller: &mut Caller<'_, ()>,
    mem: &Memory,
    method: &str,
    url: &str,
    body: Option<&str>,
    request_headers: &[(String, String)],
) -> i64 {
    match do_verb_owned(method, url, body, request_headers) {
        Some(value) => build_value(caller, mem, &value),
        None => VAL_NULL,
    }
}

/// Owned, Store-free HTTP/file work. This is safe to run on a boundary worker;
/// the caller handles guest-memory materialization after the task resumes.
#[cfg(feature = "http-client")]
fn do_verb_owned(
    method: &str,
    url: &str,
    body: Option<&str>,
    request_headers: &[(String, String)],
) -> Option<serde_json::Value> {
    // file:// parity with VM.
    if let Some(path) = file_url_to_path(url) {
        match method {
            "GET" => match std::fs::read_to_string(path) {
                Ok(content) => return Some(build_http_response_value(200, &content, &[])),
                Err(_) => return None,
            },
            "POST" | "PUT" | "PATCH" => {
                let data = body.unwrap_or("");
                match std::fs::write(path, data) {
                    Ok(_) => return Some(build_http_response_value(200, "ok", &[])),
                    Err(_) => return None,
                }
            }
            "DELETE" => match std::fs::remove_file(path) {
                Ok(_) => return Some(build_http_response_value(200, "ok", &[])),
                Err(_) => return None,
            },
            _ => return None,
        }
    }

    match do_http_request(method, url, body, request_headers) {
        Ok((status, body_text, headers)) => {
            Some(build_http_response_value(status, &body_text, &headers))
        }
        Err(_) => None,
    }
}

#[cfg(not(feature = "http-client"))]
fn do_verb_owned(
    method: &str,
    url: &str,
    body: Option<&str>,
    request_headers: &[(String, String)],
) -> Option<serde_json::Value> {
    let _ = (method, url, body, request_headers);
    None
}

pub(super) fn begin_http_request_host_op(
    caller: &mut Caller<'_, ()>,
    task_id: i32,
    op_kind: i32,
    args: &[i64],
) -> bool {
    let Some((method, has_body)) = http_method_for_host_op(op_kind) else {
        return false;
    };
    let min_args = if has_body { 2 } else { 1 };
    let max_args = min_args + 1;
    if args.len() < min_args || args.len() > max_args {
        submit_host_op(task_id, || HostOpResult::Null);
        return true;
    }
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        submit_host_op(task_id, || HostOpResult::Null);
        return true;
    };
    let url = {
        let data = mem.data(&*caller);
        read_string_value(data, args[0]).unwrap_or("").to_string()
    };
    let body = if has_body {
        let data = mem.data(&*caller);
        Some(read_string_value(data, args[1]).unwrap_or("").to_string())
    } else {
        None
    };
    let headers = match args.get(min_args) {
        Some(headers_val) => match read_headers_arg(&mem, caller, *headers_val) {
            Ok(headers) => headers,
            Err(msg) => {
                // Unresolvable Secret header — surface as the standard
                // catchable await-point error (value-free message).
                submit_host_op(task_id, move || HostOpResult::Error(msg));
                return true;
            }
        },
        None => Vec::new(),
    };
    // Wait class: server-paced, held for the whole request (plan 103 KTD2).
    submit_host_wait(task_id, move || {
        match do_verb_owned(method, &url, body.as_deref(), &headers) {
            Some(value) => HostOpResult::Json(value),
            None => HostOpResult::Null,
        }
    });
    true
}

fn http_method_for_host_op(op_kind: i32) -> Option<(&'static str, bool)> {
    match op_kind {
        fai_codegen_wasm::HOST_OP_HTTP_GET => Some(("GET", false)),
        fai_codegen_wasm::HOST_OP_HTTP_POST => Some(("POST", true)),
        fai_codegen_wasm::HOST_OP_HTTP_PUT => Some(("PUT", true)),
        fai_codegen_wasm::HOST_OP_HTTP_PATCH => Some(("PATCH", true)),
        fai_codegen_wasm::HOST_OP_HTTP_DELETE => Some(("DELETE", false)),
        _ => None,
    }
}

#[cfg(feature = "http-client")]
fn do_http_request(
    method: &str,
    url: &str,
    body: Option<&str>,
    request_headers: &[(String, String)],
) -> Result<(i32, String, Vec<(String, String)>), String> {
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(
            HTTP_CLIENT_TIMEOUT_SECS,
        )))
        .build()
        .new_agent();

    let has_content_type = request_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));

    let req_result = match (method, body) {
        ("GET", _) => {
            let mut req = agent.get(url);
            for (name, value) in request_headers {
                req = req.header(name, value);
            }
            req.call()
        }
        ("DELETE", _) => {
            let mut req = agent.delete(url);
            for (name, value) in request_headers {
                req = req.header(name, value);
            }
            req.call()
        }
        ("POST", Some(b)) => {
            let mut req = agent.post(url);
            if !has_content_type {
                req = req.header("Content-Type", "application/json");
            }
            for (name, value) in request_headers {
                req = req.header(name, value);
            }
            req.send(b.as_bytes())
        }
        ("PUT", Some(b)) => {
            let mut req = agent.put(url);
            if !has_content_type {
                req = req.header("Content-Type", "application/json");
            }
            for (name, value) in request_headers {
                req = req.header(name, value);
            }
            req.send(b.as_bytes())
        }
        ("PATCH", Some(b)) => {
            let mut req = agent.patch(url);
            if !has_content_type {
                req = req.header("Content-Type", "application/json");
            }
            for (name, value) in request_headers {
                req = req.header(name, value);
            }
            req.send(b.as_bytes())
        }
        _ => return Err(format!("invalid method/body combo: {}", method)),
    };

    match req_result {
        Ok(resp) => {
            let status = resp.status().as_u16() as i32;
            let mut headers: Vec<(String, String)> = Vec::new();
            for (name, val) in resp.headers().iter() {
                if let Ok(v) = val.to_str() {
                    headers.push((name.as_str().to_string(), v.to_string()));
                }
            }
            let body_text = resp
                .into_body()
                .read_to_string()
                .map_err(|e| format!("read response body: {}", e))?;
            Ok((status, body_text, headers))
        }
        Err(e) => Err(format!("{}", e)),
    }
}

fn read_headers_arg(
    mem: &Memory,
    caller: &mut Caller<'_, ()>,
    headers_val: i64,
) -> Result<Vec<(String, String)>, String> {
    let v = headers_val as u64;
    if v == VAL_NULL as u64 || (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return Ok(Vec::new());
    }
    let addr = (v & ADDR_MASK) as usize;
    let data = mem.data(&*caller);
    if addr + 8 > data.len() {
        return Ok(Vec::new());
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().unwrap_or([0; 4]));
    if tag != OBJ_TAG_DICT {
        return Ok(Vec::new());
    }
    let count = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().unwrap_or([0; 4])) as usize;
    let mut headers = Vec::new();
    for i in 0..count {
        let off = addr + 8 + i * 16;
        if off + 16 > data.len() {
            break;
        }
        let key = i64::from_le_bytes(data[off..off + 8].try_into().unwrap_or([0; 8]));
        let value = i64::from_le_bytes(data[off + 8..off + 16].try_into().unwrap_or([0; 8]));
        let Some(name) = read_string_value(data, key) else {
            continue;
        };
        if let Some(header_value) = read_string_value(data, value) {
            headers.push((name.to_string(), header_value.to_string()));
            continue;
        }
        // Plan 132 phase 3 — the HTTP header egress point. A Secret
        // handle in value position resolves HOST-SIDE right before the
        // bytes leave: the plaintext is spliced into the outgoing
        // request and never enters guest memory. Failure is a catchable,
        // value-free error naming the header and backend.
        if let Some(payload) = super::secrets::read_secret_name(data, value) {
            match super::secrets::resolve_secret_payload(&payload) {
                Some(resolved) => {
                    headers.push((name.to_string(), resolved));
                    continue;
                }
                None => {
                    return Err(format!(
                        "http: secret for header '{}' could not be resolved (backend {})",
                        name,
                        super::secrets::active_backend()
                    ));
                }
            }
        }
        // Non-string, non-secret values keep the old skip semantics.
    }
    Ok(headers)
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.http_post(url_ptr, url_len, body_ptr, body_len, result_buf_ptr) -> i32
    linker
        .func_wrap(
            "env",
            "http_post",
            |#[allow(unused_mut)] mut caller: Caller<'_, ()>,
             url_ptr: i32,
             url_len: i32,
             body_ptr: i32,
             body_len: i32,
             #[allow(unused_variables)] result_buf_ptr: i32|
             -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                #[allow(unused_variables)]
                let url =
                    String::from_utf8_lossy(&data[url_ptr as usize..(url_ptr + url_len) as usize])
                        .into_owned();
                #[allow(unused_variables)]
                let body = String::from_utf8_lossy(
                    &data[body_ptr as usize..(body_ptr + body_len) as usize],
                )
                .into_owned();

                #[cfg(feature = "http-client")]
                {
                    let agent = ureq::Agent::config_builder()
                        .timeout_global(Some(std::time::Duration::from_secs(
                            HTTP_CLIENT_TIMEOUT_SECS,
                        )))
                        .build()
                        .new_agent();
                    // Legacy fixed-buffer ABI: callers hand a 64 KiB scratch
                    // buffer; cap the copy so an oversized response fails
                    // cleanly instead of scribbling the guest heap (or
                    // panicking past memory end).
                    let write_capped = |caller: &mut Caller<'_, ()>, text: &str| -> i32 {
                        let bytes = text.as_bytes();
                        if bytes.len() > 65536 {
                            return -1;
                        }
                        let dest = result_buf_ptr as usize;
                        let data = mem.data_mut(&mut *caller);
                        if dest + bytes.len() > data.len() {
                            return -1;
                        }
                        data[dest..dest + bytes.len()].copy_from_slice(bytes);
                        bytes.len() as i32
                    };
                    match agent
                        .post(&url)
                        .header("Content-Type", "application/json")
                        .send(body.as_bytes())
                    {
                        Ok(resp) => {
                            let resp_body = resp.into_body().read_to_string().unwrap_or_default();
                            write_capped(&mut caller, &resp_body)
                        }
                        Err(e) => {
                            let err = format!("{{\"ok\":false,\"error\":\"{}\"}}", e);
                            write_capped(&mut caller, &err)
                        }
                    }
                }
                #[cfg(not(feature = "http-client"))]
                {
                    -1_i32
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_request_get(url_ptr, url_len, headers_val) -> i64 (Dict or null)
    linker
        .func_wrap(
            "env",
            "http_request_get",
            |mut caller: Caller<'_, ()>, url_ptr: i32, url_len: i32, headers_val: i64| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let url = read_str(&mem, &caller, url_ptr, url_len);
                let headers = match read_headers_arg(&mem, &mut caller, headers_val) {
                    Ok(headers) => headers,
                    Err(msg) => return signal_host_error(&mut caller, "secrets", &msg),
                };
                do_verb(&mut caller, &mem, "GET", &url, None, &headers)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_request_post(url_ptr, url_len, body_ptr, body_len, headers_val) -> i64
    linker
        .func_wrap(
            "env",
            "http_request_post",
            |mut caller: Caller<'_, ()>,
             url_ptr: i32,
             url_len: i32,
             body_ptr: i32,
             body_len: i32,
             headers_val: i64|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let url = read_str(&mem, &caller, url_ptr, url_len);
                let body = read_str(&mem, &caller, body_ptr, body_len);
                let headers = match read_headers_arg(&mem, &mut caller, headers_val) {
                    Ok(headers) => headers,
                    Err(msg) => return signal_host_error(&mut caller, "secrets", &msg),
                };
                do_verb(&mut caller, &mem, "POST", &url, Some(&body), &headers)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_request_put(url_ptr, url_len, body_ptr, body_len, headers_val) -> i64
    linker
        .func_wrap(
            "env",
            "http_request_put",
            |mut caller: Caller<'_, ()>,
             url_ptr: i32,
             url_len: i32,
             body_ptr: i32,
             body_len: i32,
             headers_val: i64|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let url = read_str(&mem, &caller, url_ptr, url_len);
                let body = read_str(&mem, &caller, body_ptr, body_len);
                let headers = match read_headers_arg(&mem, &mut caller, headers_val) {
                    Ok(headers) => headers,
                    Err(msg) => return signal_host_error(&mut caller, "secrets", &msg),
                };
                do_verb(&mut caller, &mem, "PUT", &url, Some(&body), &headers)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_request_patch(url_ptr, url_len, body_ptr, body_len, headers_val) -> i64
    linker
        .func_wrap(
            "env",
            "http_request_patch",
            |mut caller: Caller<'_, ()>,
             url_ptr: i32,
             url_len: i32,
             body_ptr: i32,
             body_len: i32,
             headers_val: i64|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let url = read_str(&mem, &caller, url_ptr, url_len);
                let body = read_str(&mem, &caller, body_ptr, body_len);
                let headers = match read_headers_arg(&mem, &mut caller, headers_val) {
                    Ok(headers) => headers,
                    Err(msg) => return signal_host_error(&mut caller, "secrets", &msg),
                };
                do_verb(&mut caller, &mem, "PATCH", &url, Some(&body), &headers)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_request_delete(url_ptr, url_len, headers_val) -> i64
    linker
        .func_wrap(
            "env",
            "http_request_delete",
            |mut caller: Caller<'_, ()>, url_ptr: i32, url_len: i32, headers_val: i64| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let url = read_str(&mem, &caller, url_ptr, url_len);
                let headers = match read_headers_arg(&mem, &mut caller, headers_val) {
                    Ok(headers) => headers,
                    Err(msg) => return signal_host_error(&mut caller, "secrets", &msg),
                };
                do_verb(&mut caller, &mem, "DELETE", &url, None, &headers)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_request_guarded(method,len, url,len, body,len, headers_val,
    // options_ptr,options_len) -> i64 (Response Dict or null).
    linker
        .func_wrap(
            "env",
            "http_request_guarded",
            |mut caller: Caller<'_, ()>,
             method_ptr: i32,
             method_len: i32,
             url_ptr: i32,
             url_len: i32,
             body_ptr: i32,
             body_len: i32,
             headers_val: i64,
             options_ptr: i32,
             options_len: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let method = read_str(&mem, &caller, method_ptr, method_len);
                let url = read_str(&mem, &caller, url_ptr, url_len);
                let body = read_str(&mem, &caller, body_ptr, body_len);
                let options = read_str(&mem, &caller, options_ptr, options_len);
                let headers = match read_headers_arg(&mem, &mut caller, headers_val) {
                    Ok(headers) => headers,
                    Err(msg) => return signal_host_error(&mut caller, "secrets", &msg),
                };
                let method_upper = method.to_ascii_uppercase();
                let body_opt = if matches!(method_upper.as_str(), "POST" | "PUT" | "PATCH") {
                    Some(body.as_str())
                } else {
                    None
                };
                match do_guarded_request(&method_upper, &url, body_opt, &headers, &options) {
                    Some(value) => build_value(&mut caller, &mem, &value),
                    None => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.url_encode(ptr, len) -> i64 / env.url_decode(ptr, len) -> i64.
    linker
        .func_wrap(
            "env",
            "url_encode",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let value = read_str(&mem, &caller, ptr, len);
                super::super::heap::wasm_alloc_str(&mut caller, &mem, &percent_encode_component(&value))
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;
    linker
        .func_wrap(
            "env",
            "url_decode",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let value = read_str(&mem, &caller, ptr, len);
                super::super::heap::wasm_alloc_str(&mut caller, &mem, &percent_decode(&value))
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.net_available() -> i32 (1/0). Native wasmtime always reports
    // networking available; the browser host linker stubs this to 0.
    linker
        .func_wrap("env", "net_available", |_caller: Caller<'_, ()>| -> i32 {
            1
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // env.ffi_available(name_ptr, name_len) -> i32. Checks whether the named
    // C library is reachable via pkg-config or common system paths. Mirrors
    // `discover_c_library` in fai-runtime so behaviour lines up with the VM.
    linker
        .func_wrap(
            "env",
            "ffi_available",
            |mut caller: Caller<'_, ()>, name_ptr: i32, name_len: i32| -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let name = read_str(&mem, &caller, name_ptr, name_len);
                if name.is_empty() {
                    return 0;
                }
                if discover_c_library(&name) {
                    1
                } else {
                    0
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.remote_call(url_ptr, url_len, fn_ptr, fn_len, args_ptr, args_len, hash_ptr, hash_len) -> i64
    linker
        .func_wrap(
            "env",
            "remote_call",
            |#[allow(unused_mut)] mut caller: Caller<'_, ()>,
             url_ptr: i32,
             url_len: i32,
             fn_ptr: i32,
             fn_len: i32,
             args_ptr: i32,
             args_len: i32,
             hash_ptr: i32,
             hash_len: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                #[allow(unused_variables)]
                let (url, fn_name, args_json, hash) = {
                    let data = mem.data(&caller);
                    (
                        String::from_utf8_lossy(
                            &data[url_ptr as usize..(url_ptr + url_len) as usize],
                        )
                        .into_owned(),
                        String::from_utf8_lossy(&data[fn_ptr as usize..(fn_ptr + fn_len) as usize])
                            .into_owned(),
                        String::from_utf8_lossy(
                            &data[args_ptr as usize..(args_ptr + args_len) as usize],
                        )
                        .into_owned(),
                        String::from_utf8_lossy(
                            &data[hash_ptr as usize..(hash_ptr + hash_len) as usize],
                        )
                        .into_owned(),
                    )
                };

                #[cfg(feature = "http-client")]
                {
                    let rpc_url = format!("{}/fai/rpc", url.trim_end_matches('/'));
                    let body = format!(
                        "{{\"fn\":\"{}\",\"args\":{},\"hash\":\"{}\"}}",
                        fn_name, args_json, hash
                    );
                    let agent = ureq::Agent::config_builder()
                        .timeout_global(Some(std::time::Duration::from_secs(
                            HTTP_CLIENT_TIMEOUT_SECS,
                        )))
                        .build()
                        .new_agent();
                    // Send the request and surface every failure mode as a
                    // wasm throw the caller's try/catch can recover from:
                    //   - network failure (offline, DNS, refused, timeout)
                    //   - HTTP non-2xx (4xx/5xx — server reachable but
                    //     returned an error status, possibly with a non-JSON
                    //     body like an nginx error page)
                    //   - invalid JSON (server responded but the body wasn't
                    //     parseable — protocol mismatch, truncation, etc.)
                    //   - app-level error (`{"ok":false,"error":...}`) —
                    //     pass the server's message through verbatim
                    match agent
                        .post(&rpc_url)
                        .header("Content-Type", "application/json")
                        .send(body.as_bytes())
                    {
                        Err(e) => {
                            signal_remote_call_error(&mut caller, &format!("network error: {}", e))
                        }
                        Ok(resp) => {
                            let status = resp.status().as_u16();
                            let resp_body = resp.into_body().read_to_string().unwrap_or_default();
                            if !(200..300).contains(&status) {
                                signal_remote_call_error(&mut caller, &format!("HTTP {}", status))
                            } else {
                                match serde_json::from_str::<serde_json::Value>(&resp_body) {
                                    Ok(parsed) => {
                                        if parsed
                                            .get("ok")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false)
                                        {
                                            let value = parsed
                                                .get("value")
                                                .unwrap_or(&serde_json::Value::Null);
                                            build_value(&mut caller, &mem, value)
                                        } else {
                                            let msg = parsed
                                                .get("error")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("remote call failed");
                                            signal_remote_call_error(&mut caller, msg)
                                        }
                                    }
                                    Err(_) => signal_remote_call_error(
                                        &mut caller,
                                        "invalid JSON in response",
                                    ),
                                }
                            }
                        }
                    }
                }
                #[cfg(not(feature = "http-client"))]
                {
                    VAL_NULL
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.remote_begin(task_id, url*,len, fn*,len, args*,len, hash*,len) -> ()
    // The async (suspending) RPC path. Native does the request synchronously,
    // stores the result keyed by task id, and resumes the parked task; the guest
    // then reads it with `remote_result`. (On the browser the same import is
    // implemented with async `fetch`, keeping the UI thread free.)
    linker
        .func_wrap(
            "env",
            "remote_begin",
            |#[allow(unused_mut)] mut caller: Caller<'_, ()>,
             task_id: i32,
             url_ptr: i32,
             url_len: i32,
             fn_ptr: i32,
             fn_len: i32,
             args_ptr: i32,
             args_len: i32,
             hash_ptr: i32,
             hash_len: i32| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                #[allow(unused_variables)]
                let (url, fn_name, args_json, hash) = {
                    let data = mem.data(&caller);
                    (
                        String::from_utf8_lossy(
                            &data[url_ptr as usize..(url_ptr + url_len) as usize],
                        )
                        .into_owned(),
                        String::from_utf8_lossy(&data[fn_ptr as usize..(fn_ptr + fn_len) as usize])
                            .into_owned(),
                        String::from_utf8_lossy(
                            &data[args_ptr as usize..(args_ptr + args_len) as usize],
                        )
                        .into_owned(),
                        String::from_utf8_lossy(
                            &data[hash_ptr as usize..(hash_ptr + hash_len) as usize],
                        )
                        .into_owned(),
                    )
                };
                // Offload the blocking request to a boundary waiter thread
                // (Wait: server-paced, held for the full request; plan 103
                // KTD2) and leave the task parked (plan 101 U2/U6). The async
                // lowering
                // suspended this task right after the call; the driver loop
                // pumps the boundary completion and resumes it via
                // `__fai_resume_task`, then `remote_result` reads the value. The
                // worker only touches owned data (the request strings), never
                // the Store. (The browser implements `remote_begin` in JS with
                // async `fetch`, so this native change doesn't affect it.)
                super::boundary::with_boundary(|b| {
                    b.submit(task_id, super::boundary::JobClass::Wait, move || {
                        Box::new(rpc_request_owned(url, fn_name, args_json, hash))
                            as Box<dyn std::any::Any + Send>
                    });
                });
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.remote_result(task_id) -> i64 — the value stored by `remote_begin`,
    // or null with `__error_flag` set on failure (for the caller's try/catch).
    linker
        .func_wrap(
            "env",
            "remote_result",
            |mut caller: Caller<'_, ()>, task_id: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                match super::boundary::take_ready(task_id) {
                    Some(Ok(boxed)) => {
                        match boxed.downcast::<Result<serde_json::Value, String>>() {
                            Ok(b) => match *b {
                                Ok(value) => build_value(&mut caller, &mem, &value),
                                Err(msg) => signal_remote_call_error(&mut caller, &msg),
                            },
                            Err(_) => {
                                signal_remote_call_error(&mut caller, "RPC result type mismatch")
                            }
                        }
                    }
                    Some(Err(panic_msg)) => signal_remote_call_error(&mut caller, &panic_msg),
                    None => signal_remote_call_error(&mut caller, "missing RPC result"),
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

/// Perform an RPC POST on a boundary worker thread, returning the parsed
/// `value` (or an error message) as owned data. Runs off the main thread — must
/// not touch the Store or guest memory; the caller (`remote_result`) marshals
/// the returned `serde_json::Value` into the guest heap on the main thread.
#[cfg(feature = "http-client")]
fn rpc_request_owned(
    url: String,
    fn_name: String,
    args_json: String,
    hash: String,
) -> Result<serde_json::Value, String> {
    let rpc_url = format!("{}/fai/rpc", url.trim_end_matches('/'));
    let body = format!(
        "{{\"fn\":\"{}\",\"args\":{},\"hash\":\"{}\"}}",
        fn_name, args_json, hash
    );
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(
            HTTP_CLIENT_TIMEOUT_SECS,
        )))
        .build()
        .new_agent();
    match agent
        .post(&rpc_url)
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        Err(e) => Err(format!("network error: {}", e)),
        Ok(resp) => {
            let status = resp.status().as_u16();
            let resp_body = resp.into_body().read_to_string().unwrap_or_default();
            if !(200..300).contains(&status) {
                Err(format!("HTTP {}", status))
            } else {
                match serde_json::from_str::<serde_json::Value>(&resp_body) {
                    Ok(parsed) => {
                        if parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                            Ok(parsed
                                .get("value")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null))
                        } else {
                            Err(parsed
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("remote call failed")
                                .to_string())
                        }
                    }
                    Err(_) => Err("invalid JSON in response".to_string()),
                }
            }
        }
    }
}

#[cfg(not(feature = "http-client"))]
fn rpc_request_owned(
    _url: String,
    _fn_name: String,
    _args_json: String,
    _hash: String,
) -> Result<serde_json::Value, String> {
    Ok(serde_json::Value::Null)
}

#[cfg(all(test, feature = "http-client"))]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn percent_encode_leaves_unreserved_and_escapes_rest() {
        assert_eq!(percent_encode_component("aZ0-._~"), "aZ0-._~");
        assert_eq!(percent_encode_component("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(percent_encode_component("/"), "%2F");
    }

    #[test]
    fn percent_decode_round_trips_and_handles_plus() {
        assert_eq!(percent_decode("a%20b%26c%3Dd"), "a b&c=d");
        assert_eq!(percent_decode("a+b"), "a b");
        // Malformed escape passes through literally.
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn ip_guard_blocks_private_and_loopback() {
        for s in ["127.0.0.1", "10.0.0.1", "192.168.1.1", "172.16.5.5", "169.254.1.1", "0.0.0.0", "100.64.0.1"] {
            assert!(ip_is_blocked(s.parse::<IpAddr>().unwrap()), "should block {}", s);
        }
        assert!(ip_is_blocked("::1".parse::<IpAddr>().unwrap()));
        assert!(ip_is_blocked("fe80::1".parse::<IpAddr>().unwrap()));
        assert!(ip_is_blocked("fc00::1".parse::<IpAddr>().unwrap()));
        // IPv4-mapped loopback.
        assert!(ip_is_blocked("::ffff:127.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn ip_guard_allows_public() {
        for s in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            assert!(!ip_is_blocked(s.parse::<IpAddr>().unwrap()), "should allow {}", s);
        }
        assert!(!ip_is_blocked("2606:4700:4700::1111".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn host_port_parses_scheme_default_and_explicit() {
        assert_eq!(host_port_from_url("https://api.linear.app/graphql"), Some(("api.linear.app".to_string(), 443)));
        assert_eq!(host_port_from_url("http://example.com/x?y=1"), Some(("example.com".to_string(), 80)));
        assert_eq!(host_port_from_url("https://h.example:8443/p"), Some(("h.example".to_string(), 8443)));
        assert_eq!(host_port_from_url("https://[2606:4700::1]:8443/p"), Some(("2606:4700::1".to_string(), 8443)));
    }

    #[test]
    fn guard_refuses_localhost_and_allows_resolvable_public() {
        // Loopback name resolves to 127.0.0.1 → refused.
        assert!(guard_host_public("http://localhost:3040/x").is_err());
        // Literal private IP → refused without DNS.
        assert!(guard_host_public("http://10.0.0.5/x").is_err());
    }
}
