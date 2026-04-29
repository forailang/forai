//! HTTP server host imports: `http_server_response`, `http_server_listen`.
//!
//! Mirrors the VM behaviour in `fai-runtime/src/vm.rs` (see
//! `drain_pending_bindings`, `run_event_loop`, `parse_http_request`,
//! `write_http_response`, `is_options_request`). The wasm path differs
//! from the VM in that the accept loop runs entirely inside the host
//! import — there's no scheduler — and the handler is called back into
//! wasm via `__indirect_function_table`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use wasmtime::*;

use super::super::output;

use super::super::heap::{decode_closure_header, wasm_alloc_str};
use super::super::nan_box::{
    encode_object, ADDR_MASK, OBJ_TAG_ARRAY, OBJ_TAG_DICT, QNAN, SIGN_BIT, TAG_INT, VAL_NULL,
};

// Must stay in sync with fai-codegen-wasm/src/runtime.rs
// RESPONSE_KIND_* constants.
const KIND_TEXT: i32 = 0;
const KIND_HTML: i32 = 1;
#[allow(dead_code)] // reserved discriminant — see match arm in build_response_dict
const KIND_JSON: i32 = 2;
const KIND_OK: i32 = 3;
const KIND_REDIRECT: i32 = 4;

use std::cell::RefCell;
use std::collections::HashMap;

struct WasmRoute {
    method: String,
    pattern: String,
    handler: i64, // NaN-boxed closure, or 0 for static file routes
    static_dir: Option<String>,
}

struct WasmRouter {
    routes: Vec<WasmRoute>,
}

thread_local! {
    static WASM_ROUTER_STORE: RefCell<HashMap<u32, WasmRouter>> = RefCell::new(HashMap::new());
    static WASM_NEXT_ROUTER_ID: std::cell::Cell<u32> = std::cell::Cell::new(1);
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.http_server_response(kind, status, body_ptr, body_len) -> i64
    //
    // Builds `{status: Int, body: String [, contentType: String | location: String]}`
    // on the guest heap and returns a NaN-boxed Dict pointer. Mirrors
    // the VM's native_http_server_{text,html,json,ok,redirect}.
    linker
        .func_wrap(
            "env",
            "http_server_response",
            |mut caller: Caller<'_, ()>,
             kind: i32,
             status: i32,
             body_ptr: i32,
             body_len: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let body = {
                    let data = mem.data(&caller);
                    if body_ptr < 0 || body_len < 0 {
                        String::new()
                    } else {
                        let start = body_ptr as usize;
                        let end = start.saturating_add(body_len as usize);
                        if end > data.len() {
                            String::new()
                        } else {
                            String::from_utf8_lossy(&data[start..end]).into_owned()
                        }
                    }
                };
                build_response_dict(&mut caller, &mem, kind, status, &body)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_server_listen(port, handler_val) -> void (blocks forever)
    linker
        .func_wrap(
            "env",
            "http_server_listen",
            |mut caller: Caller<'_, ()>, port: i32, handler_val: i64| {
                let addr = format!("127.0.0.1:{}", port as u16);
                let listener = match TcpListener::bind(&addr) {
                    Ok(l) => l,
                    Err(e) => {
                        output::stderr_line(&format!("error: could not listen on port {}: {}", port, e));
                        return;
                    }
                };
                for conn in listener.incoming() {
                    let stream = match conn {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    if is_options_request(&stream) {
                        write_cors_preflight(stream);
                        continue;
                    }
                    // Build the request dict on the guest heap.
                    let request_val = {
                        let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                        parse_http_request_into_guest(&mut caller, &mem, &stream)
                    };
                    // Call handler via indirect function table.
                    match invoke_handler(&mut caller, handler_val, request_val) {
                        Some(response_val) => {
                            write_http_response(&mut caller, stream, response_val);
                        }
                        None => {
                            let body = "Handler error";
                            let resp = format!(
                                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = (&stream).write_all(resp.as_bytes());
                        }
                    }
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // Browser router imports — no-ops on native/server targets.
    linker
        .func_wrap(
            "env",
            "get_location_path",
            |_caller: Caller<'_, ()>| -> i64 { super::super::nan_box::VAL_NULL },
        )
        .map_err(|e| format!("linker error: {}", e))?;
    linker
        .func_wrap(
            "env",
            "push_history_state",
            |_caller: Caller<'_, ()>, _p: i32, _l: i32| {},
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_server_router() -> i32 (router ID)
    linker
        .func_wrap("env", "http_server_router", || -> i32 {
            let id = WASM_NEXT_ROUTER_ID.with(|n| {
                let id = n.get();
                n.set(id + 1);
                id
            });
            WASM_ROUTER_STORE.with(|store| {
                store
                    .borrow_mut()
                    .insert(id, WasmRouter { routes: Vec::new() });
            });
            id as i32
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_server_router_get(id, pat_ptr, pat_len, handler_val) -> void
    linker
        .func_wrap(
            "env",
            "http_server_router_get",
            |mut caller: Caller<'_, ()>, id: i32, pat_ptr: i32, pat_len: i32, handler_val: i64| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let pattern = read_mem_str(mem.data(&caller), pat_ptr as usize, pat_len as usize);
                WASM_ROUTER_STORE.with(|store| {
                    if let Some(r) = store.borrow_mut().get_mut(&(id as u32)) {
                        r.routes.push(WasmRoute {
                            method: "GET".into(),
                            pattern,
                            handler: handler_val,
                            static_dir: None,
                        });
                    }
                });
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_server_router_post(id, pat_ptr, pat_len, handler_val) -> void
    linker
        .func_wrap(
            "env",
            "http_server_router_post",
            |mut caller: Caller<'_, ()>, id: i32, pat_ptr: i32, pat_len: i32, handler_val: i64| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let pattern = read_mem_str(mem.data(&caller), pat_ptr as usize, pat_len as usize);
                WASM_ROUTER_STORE.with(|store| {
                    if let Some(r) = store.borrow_mut().get_mut(&(id as u32)) {
                        r.routes.push(WasmRoute {
                            method: "POST".into(),
                            pattern,
                            handler: handler_val,
                            static_dir: None,
                        });
                    }
                });
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_server_router_serve_files(id, dir_ptr, dir_len) -> void
    linker
        .func_wrap(
            "env",
            "http_server_router_serve_files",
            |mut caller: Caller<'_, ()>, id: i32, dir_ptr: i32, dir_len: i32| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let dir = read_mem_str(mem.data(&caller), dir_ptr as usize, dir_len as usize);
                WASM_ROUTER_STORE.with(|store| {
                    if let Some(r) = store.borrow_mut().get_mut(&(id as u32)) {
                        r.routes.push(WasmRoute {
                            method: "GET".into(),
                            pattern: "__static__".into(),
                            handler: 0,
                            static_dir: Some(dir),
                        });
                    }
                });
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.http_server_router_listen(id, port) -> void (blocks forever)
    linker
        .func_wrap(
            "env",
            "http_server_router_listen",
            |mut caller: Caller<'_, ()>, id: i32, port: i32| {
                let addr = format!("127.0.0.1:{}", port as u16);
                let listener = match TcpListener::bind(&addr) {
                    Ok(l) => l,
                    Err(e) => {
                        output::stderr_line(&format!(
                            "error: could not listen on port {}: {}",
                            port, e
                        ));
                        return;
                    }
                };
                // The host successfully bound the port — fan out
                // `http:listening` to any subscriber that wired itself
                // up before main called `server.listen`.
                let started = build_server_started(&mut caller, port);
                super::events::dispatch_event(&mut caller, "http:listening", started);
                for conn in listener.incoming() {
                    let stream = match conn {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    if is_options_request(&stream) {
                        write_cors_preflight(stream);
                        continue;
                    }
                    // Try static file serving first (handles binary files directly).
                    // If no static file matches, fall through to the WASM handler.
                    let method_buf = peek_request_method_path(&stream);
                    if let Some((method, path)) = &method_buf {
                        if method == "GET" {
                            if let Some(static_response) =
                                try_serve_static_from_router(id as u32, path)
                            {
                                write_raw_response(stream, static_response);
                                continue;
                            }
                        }
                    }
                    let request_val = {
                        let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                        parse_http_request_into_guest(&mut caller, &mem, &stream)
                    };
                    super::events::dispatch_event(
                        &mut caller,
                        "http:beforeRequest",
                        request_val,
                    );
                    let response = dispatch_router_request(&mut caller, id as u32, request_val);
                    let pair = build_request_response(&mut caller, request_val, response);
                    super::events::dispatch_event(&mut caller, "http:afterResponse", pair);
                    // Drain any deferred events queued during the
                    // request — `http:beforeRequest` / `http:afterResponse`
                    // subscribers can `emitDeferred(...)` for fire-and-
                    // forget logging or metrics. Drain happens after
                    // afterResponse so subscribers see the response in
                    // its final shape, but before we write the wire
                    // response so a deferred subscriber that throws
                    // doesn't block the client. See Phase 5 of
                    // plans/event-system.md.
                    super::events::drain_queue(&mut caller);
                    write_http_response(&mut caller, stream, response);
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

/// Peek at the request line (method + path) without consuming the stream.
/// Returns None if peeking fails.
fn peek_request_method_path(stream: &TcpStream) -> Option<(String, String)> {
    let mut buf = [0u8; 512];
    let n = stream.peek(&mut buf).ok()?;
    let line = std::str::from_utf8(&buf[..n]).ok()?;
    let first_line = line.lines().next()?;
    let mut parts = first_line.splitn(3, ' ');
    let method = parts.next()?.to_string();
    let raw_path = parts.next()?;
    let (path, _) = raw_path.split_once('?').unwrap_or((raw_path, ""));
    Some((method, path.to_string()))
}

/// Look up static file for the given request path in the router's serveFiles dir.
/// Returns the raw response bytes (headers + body) if a file is found.
fn try_serve_static_from_router(router_id: u32, path: &str) -> Option<Vec<u8>> {
    let rel = path.trim_start_matches('/');
    if !rel.contains('.') {
        return None;
    }

    let dir = WASM_ROUTER_STORE.with(|store| {
        store.borrow().get(&router_id).and_then(|r| {
            r.routes
                .iter()
                .find(|rt| rt.pattern == "__static__")
                .and_then(|rt| rt.static_dir.clone())
        })
    })?;

    let file_path = format!("{}/{}", dir, rel);
    let content = std::fs::read(&file_path).ok()?;
    let content_type = mime_for_path(&file_path);

    let status_line = "HTTP/1.1 200 OK";
    let header = format!(
        "{}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        status_line, content_type, content.len()
    );
    let mut response = header.into_bytes();
    response.extend_from_slice(&content);
    Some(response)
}

/// Write a raw byte response directly to the TCP stream.
fn write_raw_response(mut stream: TcpStream, response: Vec<u8>) {
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

/// Read a UTF-8 string from guest memory at (ptr, len).
fn read_mem_str(data: &[u8], ptr: usize, len: usize) -> String {
    let end = ptr.saturating_add(len);
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[ptr..end]).into_owned()
}

/// Route a request through the router and return a NaN-boxed response Dict.
fn dispatch_router_request(caller: &mut Caller<'_, ()>, router_id: u32, request_val: i64) -> i64 {
    // Extract method and path from the guest request Dict.
    let method = {
        let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
        let v = request_val as u64;
        let addr = if (v & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT) {
            (v & ADDR_MASK) as usize
        } else {
            return VAL_NULL;
        };
        read_dict_string(&mem, caller, addr, "method").unwrap_or_default()
    };
    let path = {
        let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
        let v = request_val as u64;
        let addr = (v & ADDR_MASK) as usize;
        read_dict_string(&mem, caller, addr, "path").unwrap_or_else(|| "/".into())
    };

    // Snapshot the routes to avoid borrow issues.
    let routes: Vec<(String, String, i64, Option<String>)> = WASM_ROUTER_STORE.with(|store| {
        store
            .borrow()
            .get(&router_id)
            .map(|r| {
                r.routes
                    .iter()
                    .map(|rt| {
                        (
                            rt.method.clone(),
                            rt.pattern.clone(),
                            rt.handler,
                            rt.static_dir.clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    });

    for (route_method, pattern, handler, static_dir) in &routes {
        let method_matches = route_method == &method || route_method == "*";
        if !method_matches {
            continue;
        }

        if pattern == "__static__" {
            // Static files are handled directly in the accept loop for binary-safe serving.
            // If we reach here, no static file matched (handled before WASM dispatch).
            continue;
        }

        let matches = pattern == "*" || pattern == &path;
        if matches {
            match invoke_handler_with_err(caller, *handler, request_val) {
                Ok(resp) => return resp,
                Err(e) => {
                    output::stderr_line(&format!(
                        "[router] handler error for {} {}: {}",
                        method, path, e
                    ));
                    let err_payload = build_http_error(caller, request_val, &e);
                    super::events::dispatch_event(caller, "http:error", err_payload);
                    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                    return build_response_dict(
                        caller,
                        &mem,
                        KIND_TEXT,
                        500,
                        &format!("Handler error: {}", e),
                    );
                }
            }
        }
    }

    // 404
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    build_response_dict(caller, &mem, KIND_TEXT, 404, "Not Found")
}

/// Try to serve a static file; returns Some(NaN-boxed Dict) if the file exists.
/// Only serves paths with a file extension — extensionless paths are page routes
/// handled by the SSR catch-all.
fn try_serve_static_guest(caller: &mut Caller<'_, ()>, dir: &str, path: &str) -> Option<i64> {
    let rel = path.trim_start_matches('/');
    if !rel.contains('.') {
        return None;
    }
    let file_path = format!("{}/{}", dir, rel);
    let content = std::fs::read(&file_path).ok()?;
    let content_type = mime_for_path(&file_path);
    let body = String::from_utf8_lossy(&content).into_owned();
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let key_status = wasm_alloc_str(caller, &mem, "status");
    let key_body = wasm_alloc_str(caller, &mem, "body");
    let key_ct = wasm_alloc_str(caller, &mem, "contentType");
    let v_status = (QNAN | TAG_INT | 200u64) as i64;
    let v_body = wasm_alloc_str(caller, &mem, &body);
    let v_ct = wasm_alloc_str(caller, &mem, content_type);
    Some(alloc_dict(
        caller,
        &mem,
        &[(key_status, v_status), (key_body, v_body), (key_ct, v_ct)],
    ))
}

fn mime_for_path(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".js") {
        "application/javascript"
    } else if path.ends_with(".wasm") {
        "application/wasm"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else {
        "application/octet-stream"
    }
}

/// Build a response `Dict` on the guest heap. `kind` determines which
/// auxiliary fields get attached (contentType / location).
fn build_response_dict(
    caller: &mut Caller<'_, ()>,
    mem: &Memory,
    kind: i32,
    status: i32,
    body: &str,
) -> i64 {
    // Resolve status per the VM:
    // ok  → 200 (caller supplies 200 already via codegen)
    // redirect → default 302 if caller passed 0/garbage; we just trust
    //   the value the caller picked, matching the VM's
    //   `args.first().map(|v| if v.is_int() {..} else {302}).unwrap_or(302)`
    //   (the wasm codegen always extracts the i32 from the NaN-boxed
    //   Int, so 0 just stays 0 here — the user's responsibility).
    let status = if kind == KIND_OK { 200 } else { status };

    let key_status = wasm_alloc_str(caller, mem, "status");
    let key_body = wasm_alloc_str(caller, mem, "body");

    let body_val = wasm_alloc_str(caller, mem, body);
    let status_val = (QNAN | TAG_INT | (status as u32 as u64)) as i64;

    // Optional third entry (contentType or location).
    let extra: Option<(i64, i64)> = match kind {
        KIND_TEXT => {
            let k = wasm_alloc_str(caller, mem, "contentType");
            let v = wasm_alloc_str(caller, mem, "text/plain");
            Some((k, v))
        }
        KIND_HTML => {
            let k = wasm_alloc_str(caller, mem, "contentType");
            let v = wasm_alloc_str(caller, mem, "text/html; charset=utf-8");
            Some((k, v))
        }
        KIND_REDIRECT => {
            let k = wasm_alloc_str(caller, mem, "location");
            // For redirect the `body` arg is actually the URL. The
            // VM still sets body to "", so we match that.
            Some((k, body_val))
        }
        _ => None,
    };

    // For redirect, body is empty.
    let body_val_final = if kind == KIND_REDIRECT {
        wasm_alloc_str(caller, mem, "")
    } else {
        body_val
    };

    let entries: Vec<(i64, i64)> = match extra {
        Some((k, v)) => vec![(key_status, status_val), (key_body, body_val_final), (k, v)],
        None => vec![(key_status, status_val), (key_body, body_val_final)],
    };

    alloc_dict(caller, mem, &entries)
}

/// Allocate a `Dict` on the guest heap and return a NaN-boxed pointer.
fn alloc_dict(caller: &mut Caller<'_, ()>, mem: &Memory, entries: &[(i64, i64)]) -> i64 {
    let addr = heap_ptr(caller);
    let cap = std::cmp::max(entries.len(), 16);
    let data = mem.data_mut(&mut *caller);
    data[addr as usize..addr as usize + 4].copy_from_slice(&OBJ_TAG_DICT.to_le_bytes());
    data[addr as usize + 4..addr as usize + 8]
        .copy_from_slice(&(entries.len() as i32).to_le_bytes());
    for (i, (k, v)) in entries.iter().enumerate() {
        let ea = addr as usize + 8 + i * 16;
        data[ea..ea + 8].copy_from_slice(&k.to_le_bytes());
        data[ea + 8..ea + 16].copy_from_slice(&v.to_le_bytes());
    }
    let new_heap = align8(addr + 8 + cap as u32 * 16);
    set_heap_ptr(caller, new_heap);
    encode_object(addr)
}

fn heap_ptr(caller: &mut Caller<'_, ()>) -> u32 {
    let g = caller
        .get_export("__heap_ptr")
        .unwrap()
        .into_global()
        .unwrap();
    g.get(&mut *caller).unwrap_i32() as u32
}

fn set_heap_ptr(caller: &mut Caller<'_, ()>, new_heap: u32) {
    let g = caller
        .get_export("__heap_ptr")
        .unwrap()
        .into_global()
        .unwrap();
    let _ = g.set(&mut *caller, Val::I32(new_heap as i32));
}

fn align8(n: u32) -> u32 {
    (n + 7) & !7
}

/// Build a `RequestResponse { request, response }` Dict on the guest
/// heap — the `http:afterResponse` payload.
fn build_request_response(caller: &mut Caller<'_, ()>, request_val: i64, response_val: i64) -> i64 {
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let key_request = wasm_alloc_str(caller, &mem, "request");
    let key_response = wasm_alloc_str(caller, &mem, "response");
    alloc_dict(
        caller,
        &mem,
        &[(key_request, request_val), (key_response, response_val)],
    )
}

/// Build a `ServerStarted { port }` Dict on the guest heap — the
/// `http:listening` payload.
fn build_server_started(caller: &mut Caller<'_, ()>, port: i32) -> i64 {
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let key_port = wasm_alloc_str(caller, &mem, "port");
    let port_val = (QNAN | TAG_INT | (port as u32 as u64)) as i64;
    alloc_dict(caller, &mem, &[(key_port, port_val)])
}

/// Build an `HttpError { request, message }` Dict on the guest heap
/// — the `http:error` payload.
fn build_http_error(caller: &mut Caller<'_, ()>, request_val: i64, message: &str) -> i64 {
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let key_request = wasm_alloc_str(caller, &mem, "request");
    let key_message = wasm_alloc_str(caller, &mem, "message");
    let message_val = wasm_alloc_str(caller, &mem, message);
    alloc_dict(
        caller,
        &mem,
        &[(key_request, request_val), (key_message, message_val)],
    )
}

/// Peek at the first 7 bytes of the stream to detect an OPTIONS
/// preflight. Matches the VM's `is_options_request`.
fn is_options_request(stream: &TcpStream) -> bool {
    let mut buf = [0u8; 7];
    match stream.peek(&mut buf) {
        Ok(n) if n >= 7 => &buf[..7] == b"OPTIONS",
        _ => false,
    }
}

fn write_cors_preflight(mut stream: TcpStream) {
    let resp = "HTTP/1.1 204 No Content\r\n\
        Access-Control-Allow-Origin: *\r\n\
        Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
        Access-Control-Allow-Headers: Content-Type\r\n\
        Access-Control-Max-Age: 86400\r\n\
        Content-Length: 0\r\n\
        Connection: close\r\n\r\n";
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// Parse the incoming request into a `{method, path, body, headers, query}`
/// Dict on the guest heap. Mirrors VM's `parse_http_request`.
fn parse_http_request_into_guest(
    caller: &mut Caller<'_, ()>,
    mem: &Memory,
    stream: &TcpStream,
) -> i64 {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return VAL_NULL;
    }
    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return VAL_NULL;
    }
    let method = parts[0].to_string();
    let raw_path = parts[1].to_string();
    let (path, query_string) = match raw_path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (raw_path.clone(), String::new()),
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.push((key, val));
        }
    }
    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body_bytes);
    }
    let body_str = String::from_utf8_lossy(&body_bytes).into_owned();

    // Build sub-dicts for headers + query on the guest heap.
    let header_entries: Vec<(i64, i64)> = headers
        .iter()
        .map(|(k, v)| {
            let kv = wasm_alloc_str(caller, mem, k);
            let vv = wasm_alloc_str(caller, mem, v);
            (kv, vv)
        })
        .collect();
    let headers_dict = alloc_dict(caller, mem, &header_entries);

    let query_entries: Vec<(i64, i64)> = if query_string.is_empty() {
        Vec::new()
    } else {
        query_string
            .split('&')
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                let kv = wasm_alloc_str(caller, mem, k);
                let vv = wasm_alloc_str(caller, mem, v);
                (kv, vv)
            })
            .collect()
    };
    let query_dict = alloc_dict(caller, mem, &query_entries);

    // Top-level request dict.
    let k_method = wasm_alloc_str(caller, mem, "method");
    let v_method = wasm_alloc_str(caller, mem, &method);
    let k_path = wasm_alloc_str(caller, mem, "path");
    let v_path = wasm_alloc_str(caller, mem, &path);
    let k_body = wasm_alloc_str(caller, mem, "body");
    let v_body = wasm_alloc_str(caller, mem, &body_str);
    let k_headers = wasm_alloc_str(caller, mem, "headers");
    let k_query = wasm_alloc_str(caller, mem, "query");
    alloc_dict(
        caller,
        mem,
        &[
            (k_method, v_method),
            (k_path, v_path),
            (k_body, v_body),
            (k_headers, headers_dict),
            (k_query, query_dict),
        ],
    )
}

/// Look up `status`/`body`/`contentType`/`location` plus the optional
/// `cookies` and `headers` fields in a NaN-boxed `HttpResponse` Dict
/// and write an HTTP response. Cookies serialize to one `Set-Cookie:`
/// line each; headers contribute extra header lines after the
/// built-ins. Mirrors the VM's `write_http_response` for the legacy
/// fields.
fn write_http_response(caller: &mut Caller<'_, ()>, mut stream: TcpStream, response_val: i64) {
    let val = response_val as u64;
    // Must be an object pointer.
    if (val & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        let _ = stream.write_all(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    let addr = (val & ADDR_MASK) as usize;
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();

    let status = read_dict_string(&mem, caller, addr, "status")
        .and_then(|s| s.parse::<i32>().ok())
        .or_else(|| read_dict_int(&mem, caller, addr, "status"))
        .unwrap_or(200);
    let body = read_dict_string(&mem, caller, addr, "body").unwrap_or_default();
    let content_type =
        read_dict_string(&mem, caller, addr, "contentType").unwrap_or_else(|| "text/plain".into());
    let location = read_dict_string(&mem, caller, addr, "location");
    let cookie_lines = read_cookies(&mem, caller, addr);
    let extra_headers = read_extra_headers(&mem, caller, addr);

    let status_text = status_text(status);
    let mut response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: POST, GET, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\n",
        status, status_text, content_type, body.len()
    );
    if let Some(loc) = location {
        response.push_str(&format!("Location: {}\r\n", loc));
    }
    for line in &cookie_lines {
        response.push_str("Set-Cookie: ");
        response.push_str(line);
        response.push_str("\r\n");
    }
    for (name, value) in &extra_headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    response.push_str(&body);
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn status_text(status: i32) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

/// Format a single `Set-Cookie:` value (everything after the `: `).
/// Pure-Rust so the formatting can be unit-tested without standing up
/// a guest wasm instance. Skips optional attributes that the caller
/// didn't set.
fn format_cookie(
    name: &str,
    value: &str,
    path: Option<&str>,
    max_age: Option<i32>,
    http_only: Option<bool>,
    secure: Option<bool>,
    same_site: Option<&str>,
) -> String {
    let mut out = format!("{}={}", name, value);
    if let Some(p) = path {
        if !p.is_empty() {
            out.push_str("; Path=");
            out.push_str(p);
        }
    }
    if let Some(age) = max_age {
        out.push_str(&format!("; Max-Age={}", age));
    }
    if matches!(http_only, Some(true)) {
        out.push_str("; HttpOnly");
    }
    if matches!(secure, Some(true)) {
        out.push_str("; Secure");
    }
    if let Some(ss) = same_site {
        if !ss.is_empty() {
            out.push_str("; SameSite=");
            out.push_str(ss);
        }
    }
    out
}

/// Read the `cookies` field out of a response Dict and format each
/// Cookie record into a `Set-Cookie:` line value.
fn read_cookies(mem: &Memory, caller: &mut Caller<'_, ()>, addr: usize) -> Vec<String> {
    let Some(cookies_val) = dict_lookup(mem, caller, addr, "cookies") else {
        return Vec::new();
    };
    let v = cookies_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return Vec::new();
    }
    let arr_addr = (v & ADDR_MASK) as usize;
    let count = match read_array_count(mem, caller, arr_addr) {
        Some(n) => n,
        None => return Vec::new(),
    };
    let mut lines = Vec::new();
    for i in 0..count {
        let Some(item) = read_array_item(mem, caller, arr_addr, i) else {
            continue;
        };
        let item_v = item as u64;
        if (item_v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
            continue;
        }
        let cookie_addr = (item_v & ADDR_MASK) as usize;
        let name = match read_dict_string(mem, caller, cookie_addr, "name") {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let value = read_dict_string(mem, caller, cookie_addr, "value").unwrap_or_default();
        let path = read_dict_string(mem, caller, cookie_addr, "path");
        let max_age = read_dict_int(mem, caller, cookie_addr, "maxAge");
        let http_only = read_dict_bool(mem, caller, cookie_addr, "httpOnly");
        let secure = read_dict_bool(mem, caller, cookie_addr, "secure");
        let same_site = read_dict_string(mem, caller, cookie_addr, "sameSite");
        lines.push(format_cookie(
            &name,
            &value,
            path.as_deref(),
            max_age,
            http_only,
            secure,
            same_site.as_deref(),
        ));
    }
    lines
}

/// Read the optional `headers` Dictionary off the response Dict and
/// return its `(name, value)` pairs in iteration order. Non-string
/// values are skipped.
fn read_extra_headers(
    mem: &Memory,
    caller: &mut Caller<'_, ()>,
    addr: usize,
) -> Vec<(String, String)> {
    let Some(headers_val) = dict_lookup(mem, caller, addr, "headers") else {
        return Vec::new();
    };
    let v = headers_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return Vec::new();
    }
    let inner_addr = (v & ADDR_MASK) as usize;
    let data = mem.data(&*caller);
    if inner_addr + 8 > data.len() {
        return Vec::new();
    }
    let tag = i32::from_le_bytes(data[inner_addr..inner_addr + 4].try_into().unwrap_or([0; 4]));
    if tag != OBJ_TAG_DICT {
        return Vec::new();
    }
    let count = i32::from_le_bytes(
        data[inner_addr + 4..inner_addr + 8]
            .try_into()
            .unwrap_or([0; 4]),
    ) as usize;
    let mut out = Vec::new();
    for i in 0..count {
        let ea = inner_addr + 8 + i * 16;
        if ea + 16 > data.len() {
            break;
        }
        let k = i64::from_le_bytes(data[ea..ea + 8].try_into().unwrap_or([0; 8]));
        let v = i64::from_le_bytes(data[ea + 8..ea + 16].try_into().unwrap_or([0; 8]));
        let kv = k as u64;
        if (kv & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
            continue;
        }
        let kaddr = (kv & ADDR_MASK) as usize;
        let Some(name) = read_string_bytes(mem.data(&*caller), kaddr) else {
            continue;
        };
        let vv = v as u64;
        if (vv & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
            continue;
        }
        let vaddr = (vv & ADDR_MASK) as usize;
        let Some(value) = read_string_bytes(mem.data(&*caller), vaddr) else {
            continue;
        };
        out.push((name.to_string(), value.to_string()));
    }
    out
}

fn read_array_count(mem: &Memory, caller: &mut Caller<'_, ()>, addr: usize) -> Option<usize> {
    let data = mem.data(&*caller);
    if addr + 8 > data.len() {
        return None;
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().ok()?);
    if tag != OBJ_TAG_ARRAY {
        return None;
    }
    Some(i32::from_le_bytes(data[addr + 4..addr + 8].try_into().ok()?) as usize)
}

fn read_array_item(mem: &Memory, caller: &mut Caller<'_, ()>, addr: usize, i: usize) -> Option<i64> {
    let data = mem.data(&*caller);
    let off = addr + 8 + i * 8;
    if off + 8 > data.len() {
        return None;
    }
    Some(i64::from_le_bytes(data[off..off + 8].try_into().ok()?))
}

fn read_dict_bool(mem: &Memory, caller: &mut Caller<'_, ()>, addr: usize, key: &str) -> Option<bool> {
    let val = dict_lookup(mem, caller, addr, key)?;
    let v = val as u64;
    if (v & (QNAN | SIGN_BIT | 0x0007_0000_0000_0000)) == (QNAN | crate::wasm_runner::nan_box::TAG_BOOL) {
        Some((v & 1) == 1)
    } else {
        None
    }
}

/// Look up a key in a guest-heap Dict and, if the value is a String,
/// return its UTF-8-lossy copy.
fn read_dict_string(
    mem: &Memory,
    caller: &mut Caller<'_, ()>,
    addr: usize,
    key: &str,
) -> Option<String> {
    let entry = dict_lookup(mem, caller, addr, key)?;
    read_string_value(mem, caller, entry)
}

/// Look up a key in a guest-heap Dict and, if the value is an Int,
/// return it.
fn read_dict_int(mem: &Memory, caller: &mut Caller<'_, ()>, addr: usize, key: &str) -> Option<i32> {
    let val = dict_lookup(mem, caller, addr, key)?;
    let v = val as u64;
    if (v & (QNAN | SIGN_BIT | 0x0007_0000_0000_0000)) == (QNAN | TAG_INT) {
        Some(v as i32)
    } else {
        None
    }
}

/// Walk a Dict's entry table looking for `key`. Returns the raw
/// NaN-boxed value or None.
fn dict_lookup(mem: &Memory, caller: &mut Caller<'_, ()>, addr: usize, key: &str) -> Option<i64> {
    let data = mem.data(&*caller);
    if addr + 8 > data.len() {
        return None;
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().ok()?);
    if tag != OBJ_TAG_DICT {
        return None;
    }
    let count = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().ok()?) as usize;
    for i in 0..count {
        let ea = addr + 8 + i * 16;
        if ea + 16 > data.len() {
            break;
        }
        let k = i64::from_le_bytes(data[ea..ea + 8].try_into().ok()?);
        let v = i64::from_le_bytes(data[ea + 8..ea + 16].try_into().ok()?);
        // Key must be an object pointer (string).
        let kv = k as u64;
        if (kv & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
            continue;
        }
        let kaddr = (kv & ADDR_MASK) as usize;
        if let Some(ks) = read_string_bytes(data, kaddr) {
            if ks == key {
                return Some(v);
            }
        }
    }
    None
}

fn read_string_value(mem: &Memory, caller: &mut Caller<'_, ()>, val: i64) -> Option<String> {
    let v = val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let addr = (v & ADDR_MASK) as usize;
    let data = mem.data(&*caller);
    read_string_bytes(data, addr).map(|s| s.to_string())
}

fn read_string_bytes(data: &[u8], addr: usize) -> Option<&str> {
    if addr + 8 > data.len() {
        return None;
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().ok()?);
    if tag != 0 {
        // OBJ_TAG_STRING == 0
        return None;
    }
    let len = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().ok()?) as usize;
    let start = addr + 8;
    let end = start.checked_add(len)?;
    if end > data.len() {
        return None;
    }
    std::str::from_utf8(&data[start..end]).ok()
}

/// Invoke a handler closure with one argument, returning the error message on failure.
fn invoke_handler_with_err(
    caller: &mut Caller<'_, ()>,
    handler_val: i64,
    arg: i64,
) -> Result<i64, String> {
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let v = handler_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return Err(format!("handler is not an object (val={:#x})", v));
    }
    let addr = (v & ADDR_MASK) as usize;
    let header = {
        let data = mem.data(&*caller);
        decode_closure_header(data, addr).ok_or_else(|| {
            // Check what tag the object has
            let tag = if addr + 4 <= data.len() {
                i32::from_le_bytes(data[addr..addr + 4].try_into().unwrap_or([0, 0, 0, 0]))
            } else {
                -1
            };
            format!("not a closure at addr {:#x}, tag={}", addr, tag)
        })?
    };
    if let Some(env_global) = caller.get_export("__env_ptr") {
        if let Some(g) = env_global.into_global() {
            let _ = g.set(&mut *caller, Val::I32(header.env_addr));
        }
    }
    let table = caller
        .get_export("__indirect_function_table")
        .ok_or_else(|| "no __indirect_function_table".to_string())?
        .into_table()
        .ok_or_else(|| "__indirect_function_table is not a table".to_string())?;
    let func_ref = table
        .get(&mut *caller, header.table_idx as u64)
        .ok_or_else(|| format!("no function at table index {}", header.table_idx))?;
    let func = func_ref
        .unwrap_func()
        .ok_or_else(|| "table entry is not a func ref".to_string())?
        .clone();
    let mut results = vec![Val::I64(0)];
    func.call(&mut *caller, &[Val::I64(arg)], &mut results)
        .map_err(|e| format!("wasm trap: {}", e))?;
    match results[0] {
        Val::I64(v) => Ok(v),
        _ => Err("unexpected result type".to_string()),
    }
}

/// Invoke a handler closure with one argument (the request Dict value).
fn invoke_handler(caller: &mut Caller<'_, ()>, handler_val: i64, arg: i64) -> Option<i64> {
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let v = handler_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let addr = (v & ADDR_MASK) as usize;
    let header = {
        let data = mem.data(&*caller);
        decode_closure_header(data, addr)?
    };

    // Set __env_ptr so the closure can access upvalues.
    if let Some(env_global) = caller.get_export("__env_ptr") {
        if let Some(g) = env_global.into_global() {
            let _ = g.set(&mut *caller, Val::I32(header.env_addr));
        }
    }

    let table = caller
        .get_export("__indirect_function_table")?
        .into_table()?;
    let func_ref = table.get(&mut *caller, header.table_idx as u64)?;
    let func = func_ref.unwrap_func()?.clone();
    let mut results = vec![Val::I64(0)];
    match func.call(&mut *caller, &[Val::I64(arg)], &mut results) {
        Ok(()) => match results[0] {
            Val::I64(v) => Some(v),
            _ => None,
        },
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    //! Pure-Rust tests for the HttpResponse serializer's helpers.
    //! The wasmtime-driven path (Dict reading + handler invocation)
    //! is exercised via the `tests/fixtures/language/http_server/`
    //! fixtures.

    use super::*;

    #[test]
    fn format_cookie_minimum_is_name_value_only() {
        let line = format_cookie("session", "tok-1", None, None, None, None, None);
        assert_eq!(line, "session=tok-1");
    }

    #[test]
    fn format_cookie_includes_path_when_set() {
        let line = format_cookie("session", "tok", Some("/"), None, None, None, None);
        assert_eq!(line, "session=tok; Path=/");
    }

    #[test]
    fn format_cookie_skips_empty_path() {
        let line = format_cookie("session", "tok", Some(""), None, None, None, None);
        assert_eq!(line, "session=tok");
    }

    #[test]
    fn format_cookie_includes_max_age_when_set() {
        let line = format_cookie("session", "tok", None, Some(3600), None, None, None);
        assert_eq!(line, "session=tok; Max-Age=3600");
    }

    #[test]
    fn format_cookie_emits_http_only_only_when_true() {
        let yes = format_cookie("a", "b", None, None, Some(true), None, None);
        let no = format_cookie("a", "b", None, None, Some(false), None, None);
        let absent = format_cookie("a", "b", None, None, None, None, None);
        assert_eq!(yes, "a=b; HttpOnly");
        assert_eq!(no, "a=b");
        assert_eq!(absent, "a=b");
    }

    #[test]
    fn format_cookie_emits_secure_only_when_true() {
        let yes = format_cookie("a", "b", None, None, None, Some(true), None);
        let no = format_cookie("a", "b", None, None, None, Some(false), None);
        assert_eq!(yes, "a=b; Secure");
        assert_eq!(no, "a=b");
    }

    #[test]
    fn format_cookie_includes_same_site_value() {
        let line = format_cookie("a", "b", None, None, None, None, Some("Lax"));
        assert_eq!(line, "a=b; SameSite=Lax");
    }

    #[test]
    fn format_cookie_skips_empty_same_site() {
        let line = format_cookie("a", "b", None, None, None, None, Some(""));
        assert_eq!(line, "a=b");
    }

    #[test]
    fn format_cookie_combines_every_attribute_in_canonical_order() {
        let line = format_cookie(
            "session",
            "tok",
            Some("/"),
            Some(3600),
            Some(true),
            Some(true),
            Some("Strict"),
        );
        assert_eq!(
            line,
            "session=tok; Path=/; Max-Age=3600; HttpOnly; Secure; SameSite=Strict"
        );
    }

    #[test]
    fn status_text_covers_common_codes() {
        assert_eq!(status_text(200), "OK");
        assert_eq!(status_text(201), "Created");
        assert_eq!(status_text(204), "No Content");
        assert_eq!(status_text(301), "Moved Permanently");
        assert_eq!(status_text(302), "Found");
        assert_eq!(status_text(400), "Bad Request");
        assert_eq!(status_text(401), "Unauthorized");
        assert_eq!(status_text(404), "Not Found");
        assert_eq!(status_text(500), "Internal Server Error");
    }

    #[test]
    fn status_text_falls_back_to_ok_for_unknown_codes() {
        assert_eq!(status_text(418), "OK");
        assert_eq!(status_text(599), "OK");
    }
}
