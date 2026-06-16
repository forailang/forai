//! HTTP server host imports for typed responses and the router listener.
//!
//! Mirrors the VM behaviour in `fai-runtime/src/vm.rs` (see
//! `drain_pending_bindings`, `run_event_loop`, `parse_http_request`,
//! `write_http_response`, `is_options_request`). The wasm path differs
//! from the VM in that the accept loop runs entirely inside the host
//! import, while async route handlers are driven by the guest scheduler.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::time::Duration;

use wasmtime::*;

use super::super::output;

use super::super::heap::{decode_closure_header, host_retain_value, reserve, wasm_alloc_str};
use super::super::nan_box::{
    encode_object, ADDR_MASK, OBJ_TAG_ARRAY, OBJ_TAG_DICT, QNAN, SIGN_BIT, TAG_BOOL, TAG_INT,
    TAG_MASK, VAL_NULL, VAL_VOID,
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
    linker
        .func_wrap(
            "env",
            "replace_location",
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
                        // The router keeps the handler closure for the life of
                        // the server, so it must co-own it: retain on store,
                        // release on router teardown/reset.
                        host_retain_value(&mut caller, handler_val);
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
                        host_retain_value(&mut caller, handler_val);
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
                // Bind to all interfaces so the same server reachable via
                // localhost is also reachable via 127.0.0.1, the LAN IP, etc.
                // Cookies set by the server scope to the host the request
                // arrived on, so they match same-origin requests from the
                // browser regardless of which hostname the user typed.
                let addr = format!("0.0.0.0:{}", port as u16);
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
                // The payload is host-owned; its dispatch is over.
                host_release_value(&mut caller, started);
                // Unified driver loop (plan 101 U3/U4). Accept connections
                // without blocking, spawn each async handler as a scheduler
                // task, poll the scheduler to advance every in-flight handler,
                // and write each connection's response when its task completes.
                // Sync handlers and 404s still resolve inline. The effect: while
                // one handler awaits I/O (sleep, a DB query, a fetch), the others
                // keep running on this single thread instead of waiting in line.
                // FAI_HTTP_MAX_REQUESTS bounds how many connections to accept
                // before draining in-flight work and returning (so the program
                // exits and the runner's --check-leaks/ownership report runs).
                // Unset → serve forever, the normal case.
                let max_requests: Option<u64> = std::env::var("FAI_HTTP_MAX_REQUESTS")
                    .ok()
                    .and_then(|v| v.parse().ok());
                let mut accepted: u64 = 0;
                let mut pending_connections: Vec<TcpStream> = Vec::new();
                let mut pending: Vec<PendingRequest> = Vec::new();
                let _ = listener.set_nonblocking(true);
                loop {
                    let accepting = max_requests.map_or(true, |m| accepted < m);

                    // 1. Drain every connection ready right now (while accepting).
                    if accepting {
                        loop {
                            match listener.accept() {
                                Ok((stream, _)) => {
                                    pending_connections.push(stream);
                                    accepted += 1;
                                }
                                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(_) => break,
                            }
                        }
                    }
                    process_ready_connections(
                        &mut caller,
                        id as u32,
                        &mut pending_connections,
                        &mut pending,
                    );

                    // 2. Request cap reached and all in-flight work drained →
                    // exit the server loop so the program can terminate.
                    if !accepting && pending_connections.is_empty() && pending.is_empty() {
                        break;
                    }

                    // 3. Idle but still accepting: block for the next connection
                    // rather than spinning, then loop back to the drain.
                    if pending_connections.is_empty()
                        && pending.is_empty()
                        && guest_live_count(&mut caller) <= 1
                    {
                        let _ = listener.set_nonblocking(false);
                        if let Ok((stream, _)) = listener.accept() {
                            let _ = listener.set_nonblocking(true);
                            pending_connections.push(stream);
                            accepted += 1;
                        } else {
                            let _ = listener.set_nonblocking(true);
                        }
                        continue;
                    }

                    // 4. Advance in-flight handler tasks: run ready ones, resume
                    // any whose offloaded boundary work finished (outbound RPC,
                    // etc.), then write responses for tasks that completed.
                    let _ = guest_poll(&mut caller);
                    for task_id in super::boundary::pump_ready() {
                        guest_resume_task(&mut caller, task_id);
                    }
                    finish_completed(&mut caller, &mut pending);

                    // 5. While handlers remain parked (on a sleep timer or a
                    // boundary job), poll again shortly without a hot spin.
                    if !pending.is_empty() {
                        std::thread::sleep(Duration::from_millis(1));
                    } else if !pending_connections.is_empty() {
                        std::thread::sleep(Duration::from_millis(1));
                    } else if guest_live_count(&mut caller) > 1 {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

/// A request whose async handler is running as a scheduler task; its response
/// is written to `stream` once `task_id` completes (plan 101 U4).
struct PendingRequest {
    stream: std::net::TcpStream,
    request_val: i64,
    task_id: i32,
}

enum ConnectionReadiness {
    Ready,
    Pending,
    Closed,
}

// Task status words — mirror fai_codegen_wasm::async_engine ST_* (stable ABI).
const ST_COMPLETE: i32 = 3;
const ST_FAILED: i32 = 4;

/// Move accepted sockets into request handling only after the client has sent
/// at least one byte. Browsers may open speculative/preconnect sockets and keep
/// them idle; blocking on those sockets would stall later real requests on this
/// single runtime thread.
fn process_ready_connections(
    caller: &mut Caller<'_, ()>,
    router_id: u32,
    pending_connections: &mut Vec<TcpStream>,
    pending: &mut Vec<PendingRequest>,
) {
    let mut i = 0;
    while i < pending_connections.len() {
        match connection_readiness(&pending_connections[i]) {
            ConnectionReadiness::Ready => {
                let stream = pending_connections.swap_remove(i);
                accept_connection(caller, router_id, stream, pending);
            }
            ConnectionReadiness::Pending => {
                i += 1;
            }
            ConnectionReadiness::Closed => {
                let _ = pending_connections.swap_remove(i);
            }
        }
    }
}

fn connection_readiness(stream: &TcpStream) -> ConnectionReadiness {
    let _ = stream.set_nonblocking(true);
    let mut buf = [0u8; 1];
    match stream.peek(&mut buf) {
        Ok(0) => ConnectionReadiness::Closed,
        Ok(_) => {
            let _ = stream.set_nonblocking(false);
            ConnectionReadiness::Ready
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => ConnectionReadiness::Pending,
        Err(_) => ConnectionReadiness::Closed,
    }
}

/// Accept one connection. OPTIONS preflight and static files are served inline.
/// Otherwise the request is parsed; an async handler is spawned as a scheduler
/// task (its response written later, when the task completes), while a sync
/// handler or a 404 resolves inline exactly as before.
fn accept_connection(
    caller: &mut Caller<'_, ()>,
    router_id: u32,
    stream: std::net::TcpStream,
    pending: &mut Vec<PendingRequest>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    if is_options_request(&stream) {
        drain_request(&stream);
        write_cors_preflight(stream);
        return;
    }
    // Static files first (binary-safe direct serving); else the WASM handler.
    let method_buf = peek_request_method_path(&stream);
    if let Some((method, path)) = &method_buf {
        if method == "GET" {
            if let Some(static_response) = try_serve_static_from_router(router_id, path) {
                drain_request(&stream);
                write_raw_response(stream, static_response);
                return;
            }
        }
    }
    let request_val = {
        let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
        parse_http_request_into_guest(caller, &mem, &stream)
    };
    super::events::dispatch_event(caller, "http:beforeRequest", request_val);

    // Async handler → spawn as a task and defer its response. Sync handlers,
    // 404s, and handler errors all resolve inline via dispatch_router_request.
    if let Some(handler) = find_matching_handler(caller, router_id, request_val) {
        if handler_is_async(caller, handler) {
            if let Some(task_id) = spawn_handler(caller, handler, request_val) {
                pending.push(PendingRequest {
                    stream,
                    request_val,
                    task_id,
                });
                return;
            }
            // Spawn failed: fall through to inline dispatch as a safety net.
        }
    }
    let response = dispatch_router_request(caller, router_id, request_val);
    complete_request(caller, stream, request_val, response);
}

/// Write the response for every pending request whose handler task finished
/// this poll, and reclaim its slot. A failed task answers 500 rather than
/// writing its non-response result value.
fn finish_completed(caller: &mut Caller<'_, ()>, pending: &mut Vec<PendingRequest>) {
    let mut i = 0;
    while i < pending.len() {
        let status = guest_task_status(caller, pending[i].task_id);
        if status >= ST_COMPLETE {
            let p = pending.swap_remove(i);
            let response = if status == ST_FAILED {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let error_val = guest_task_result(caller, p.task_id);
                let error = describe_guest_error(caller, &mem, error_val);
                let (method, path) = request_method_path(caller, &mem, p.request_val);
                output::stderr_line(&format!(
                    "[router] handler error for {} {}: {}",
                    method, path, error
                ));
                let err_payload = build_http_error(caller, p.request_val, &error);
                super::events::dispatch_event(caller, "http:error", err_payload);
                host_release_value(caller, err_payload);
                let response = build_response_dict(
                    caller,
                    &mem,
                    KIND_TEXT,
                    500,
                    &format!("Handler error: {}", error),
                );
                host_release_value(caller, error_val);
                response
            } else {
                guest_task_result(caller, p.task_id)
            };
            complete_request(caller, p.stream, p.request_val, response);
            // Slot was marked host-driven, so reclaim it ourselves now that we
            // have read the result (mirrors __fai_drive_closure's inline free).
            guest_free_task(caller, p.task_id);
        } else {
            i += 1;
        }
    }
}

/// The afterResponse → drain → write → reclaim sequence, shared by inline and
/// task-completed requests. `pair` co-owns request_val + response, so releasing
/// it deep-frees the per-request graph (plan 115). Must run after the bytes are
/// written.
fn complete_request(
    caller: &mut Caller<'_, ()>,
    stream: std::net::TcpStream,
    request_val: i64,
    response: i64,
) {
    let pair = build_request_response(caller, request_val, response);
    super::events::dispatch_event(caller, "http:afterResponse", pair);
    // Deferred events (emitDeferred) flush after afterResponse sees the final
    // response shape, but before the wire write so a throwing subscriber can't
    // block the client. See plans/event-system.md Phase 5.
    super::events::drain_queue(caller);
    write_http_response(caller, stream, response);
    host_release_value(caller, pair);
}

/// The handler closure for the first route matching the request's method and
/// path, or None (→ inline 404). Mirrors dispatch_router_request's matching
/// without invoking, so the caller can choose spawn-vs-inline.
fn find_matching_handler(
    caller: &mut Caller<'_, ()>,
    router_id: u32,
    request_val: i64,
) -> Option<i64> {
    let (method, path) = {
        let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
        let v = request_val as u64;
        if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
            return None;
        }
        let addr = (v & ADDR_MASK) as usize;
        (
            read_dict_string(&mem, caller, addr, "method").unwrap_or_default(),
            read_dict_string(&mem, caller, addr, "path").unwrap_or_else(|| "/".into()),
        )
    };
    let routes: Vec<(String, String, i64)> = WASM_ROUTER_STORE.with(|store| {
        store
            .borrow()
            .get(&router_id)
            .map(|r| {
                r.routes
                    .iter()
                    .map(|rt| (rt.method.clone(), rt.pattern.clone(), rt.handler))
                    .collect()
            })
            .unwrap_or_default()
    });
    for (route_method, pattern, handler) in &routes {
        let method_matches = route_method == &method || route_method == "*";
        if !method_matches || pattern == "__static__" {
            continue;
        }
        if pattern == "*" || pattern == &path {
            return Some(*handler);
        }
    }
    None
}

/// True if the closure is an async resume fn (`frame_size > 0`), which must be
/// spawned as a task rather than called directly.
fn handler_is_async(caller: &mut Caller<'_, ()>, handler_val: i64) -> bool {
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        return false;
    };
    let v = handler_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return false;
    }
    let addr = (v & ADDR_MASK) as usize;
    let data = mem.data(&*caller);
    decode_closure_header(data, addr)
        .map(|h| h.frame_size > 0)
        .unwrap_or(false)
}

fn request_method_path(
    caller: &mut Caller<'_, ()>,
    mem: &Memory,
    request_val: i64,
) -> (String, String) {
    let v = request_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return ("?".into(), "?".into());
    }
    let addr = (v & ADDR_MASK) as usize;
    (
        read_dict_string(mem, caller, addr, "method").unwrap_or_else(|| "?".into()),
        read_dict_string(mem, caller, addr, "path").unwrap_or_else(|| "?".into()),
    )
}

fn describe_guest_error(caller: &mut Caller<'_, ()>, mem: &Memory, val: i64) -> String {
    if let Some(message) = read_string_value(mem, caller, val) {
        return message;
    }

    let v = val as u64;
    if (v & (QNAN | SIGN_BIT | TAG_MASK)) == (QNAN | TAG_INT) {
        return (v as i32).to_string();
    }
    if (v & (QNAN | SIGN_BIT | TAG_MASK)) == (QNAN | TAG_BOOL) {
        return if (v & 1) == 1 { "true" } else { "false" }.into();
    }
    if v == VAL_NULL as u64 {
        return "null".into();
    }
    if v == VAL_VOID as u64 {
        return "void".into();
    }
    if (v & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT) {
        let addr = (v & ADDR_MASK) as usize;
        if let Some(message) = read_dict_string(mem, caller, addr, "message") {
            return message;
        }
    }
    format!("0x{v:016x}")
}

/// Run one scheduler poll cycle (advances every READY task once).
fn guest_poll(caller: &mut Caller<'_, ()>) -> i32 {
    if let Some(f) = caller.get_export("__fai_poll").and_then(|e| e.into_func()) {
        let mut out = [Val::I32(0)];
        if f.call(&mut *caller, &[], &mut out).is_ok() {
            if let Val::I32(v) = out[0] {
                return v;
            }
        }
    }
    0
}

/// Number of live guest scheduler tasks. While inside `server.listen`, the root
/// task itself is live, so values above one mean detached/background work exists.
fn guest_live_count(caller: &mut Caller<'_, ()>) -> i32 {
    caller
        .get_export("__dbg_live")
        .and_then(|e| e.into_global())
        .and_then(|g| match g.get(&mut *caller) {
            Val::I32(v) => Some(v),
            _ => None,
        })
        .unwrap_or(0)
}

/// Mark a parked task READY (e.g. after its boundary job finished) so the next
/// poll runs its continuation.
fn guest_resume_task(caller: &mut Caller<'_, ()>, id: i32) {
    if let Some(f) = caller
        .get_export("__fai_resume_task")
        .and_then(|e| e.into_func())
    {
        let _ = f.call(&mut *caller, &[Val::I32(id)], &mut [Val::I32(0)]);
    }
}

/// Spawn an async handler closure as a scheduler task, returning its id.
fn spawn_handler(caller: &mut Caller<'_, ()>, handler: i64, arg: i64) -> Option<i32> {
    let f = caller
        .get_export("__fai_spawn_closure")
        .and_then(|e| e.into_func())?;
    let mut out = [Val::I64(0)];
    f.call(&mut *caller, &[Val::I64(handler), Val::I64(arg)], &mut out)
        .ok()?;
    match out[0] {
        Val::I64(v) => Some(v as i32),
        _ => None,
    }
}

/// A spawned task's status word; treats a missing export or trap as FAILED so a
/// wedged task still gets a 500 rather than hanging the connection.
fn guest_task_status(caller: &mut Caller<'_, ()>, id: i32) -> i32 {
    let Some(f) = caller
        .get_export("__fai_task_status")
        .and_then(|e| e.into_func())
    else {
        return ST_FAILED;
    };
    let mut out = [Val::I32(0)];
    if f.call(&mut *caller, &[Val::I32(id)], &mut out).is_err() {
        return ST_FAILED;
    }
    match out[0] {
        Val::I32(v) => v,
        _ => ST_FAILED,
    }
}

/// A completed task's NaN-boxed result value.
fn guest_task_result(caller: &mut Caller<'_, ()>, id: i32) -> i64 {
    let Some(f) = caller
        .get_export("__fai_task_result")
        .and_then(|e| e.into_func())
    else {
        return VAL_NULL;
    };
    let mut out = [Val::I64(0)];
    if f.call(&mut *caller, &[Val::I32(id)], &mut out).is_err() {
        return VAL_NULL;
    }
    match out[0] {
        Val::I64(v) => v,
        _ => VAL_NULL,
    }
}

/// Recycle a host-driven task's slot once its result has been read.
fn guest_free_task(caller: &mut Caller<'_, ()>, id: i32) {
    if let Some(f) = caller
        .get_export("__fai_free_task")
        .and_then(|e| e.into_func())
    {
        let _ = f.call(&mut *caller, &[Val::I32(id)], &mut []);
    }
}

/// Drain router-owned guest handles for test teardown and finite run cleanup.
/// Static routes use handler 0 and are ignored.
pub(crate) fn drain_retained_values() -> Vec<i64> {
    WASM_NEXT_ROUTER_ID.with(|next| next.set(1));
    WASM_ROUTER_STORE.with(|store| {
        store
            .borrow_mut()
            .drain()
            .flat_map(|(_, router)| router.routes.into_iter())
            .filter_map(|route| (route.handler != 0).then_some(route.handler))
            .collect()
    })
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

/// Consume the request line, headers, and any declared body off the
/// stream so the kernel's receive buffer is empty by the time the
/// response writer closes the socket. Required for the static-file
/// and CORS-preflight paths — they don't otherwise read the request,
/// and on Linux `close()` on a socket with unread receive-buffer data
/// sends a RST instead of a FIN. Behind Fly's edge proxy that RST
/// arrives mid-stream and the client sees a truncated body with a
/// Content-Length mismatch (Chrome: `ERR_HTTP2_PROTOCOL_ERROR`).
/// Errors are ignored — we're only draining to clean up TCP, the
/// response has already been resolved at the call site.
fn drain_request(stream: &TcpStream) {
    let mut reader = BufReader::new(stream);
    let mut content_length: usize = 0;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
    }
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
fn write_raw_response(stream: TcpStream, response: Vec<u8>) {
    finish_response(stream, &response);
}

// Per-request reclamation (plan 115/116): release host-built guest graphs
// via the shared helper (exported `__fai_release` → rt_release).
use super::super::heap::host_release_value;

/// Write the response bytes and shut the connection down gracefully.
///
/// `write_all` only guarantees the bytes hit the kernel send buffer;
/// dropping the `TcpStream` immediately afterwards calls `close()`,
/// which under load-balancer-fronted setups (Fly's edge proxy is
/// one such) can drop the in-flight tail of the response — the
/// client sees a `Content-Length` mismatch and Chrome surfaces
/// `ERR_HTTP2_PROTOCOL_ERROR`. The explicit `shutdown(Write)` sends
/// a clean FIN only after all queued data has been buffered, so the
/// kernel won't deliver the FIN until the tail is acknowledged and
/// the peer always sees a graceful end-of-stream. `set_nodelay`
/// keeps the final segment from sitting in Nagle's buffer.
fn finish_response(mut stream: TcpStream, response: &[u8]) {
    let _ = stream.set_nodelay(true);
    let _ = stream.write_all(response);
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
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

    for (route_method, pattern, handler, _static_dir) in &routes {
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
                    // `build_http_error` co-owns `request_val` for the
                    // payload's lifetime so releasing it after dispatch
                    // can't free the request the accept loop still owns.
                    let err_payload = build_http_error(caller, request_val, &e);
                    super::events::dispatch_event(caller, "http:error", err_payload);
                    host_release_value(caller, err_payload);
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
    let cap = std::cmp::max(entries.len(), 16);
    // Refcount-prefixed reserve (plan 113): writes the rc=1 prefix, grows memory
    // through the full `cap*16` extent, and returns the logical dict pointer
    // (tag@0). Replaces the old direct heap-bump (which also fixed the
    // boundary-overrun crash by growing before writing).
    let addr = reserve(caller, mem, 8 + cap * 16) as usize;
    let data = mem.data_mut(&mut *caller);
    data[addr..addr + 4].copy_from_slice(&OBJ_TAG_DICT.to_le_bytes());
    data[addr + 4..addr + 8].copy_from_slice(&(entries.len() as i32).to_le_bytes());
    for (i, (k, v)) in entries.iter().enumerate() {
        let ea = addr + 8 + i * 16;
        data[ea..ea + 8].copy_from_slice(&k.to_le_bytes());
        data[ea + 8..ea + 16].copy_from_slice(&v.to_le_bytes());
    }
    encode_object(addr as u32)
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
/// — the `http:error` payload. CO-OWNS `request_val` (host_retain) so the
/// caller can release the payload after dispatch without freeing the
/// request the accept loop still owns.
fn build_http_error(caller: &mut Caller<'_, ()>, request_val: i64, message: &str) -> i64 {
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let key_request = wasm_alloc_str(caller, &mem, "request");
    let key_message = wasm_alloc_str(caller, &mem, "message");
    let message_val = wasm_alloc_str(caller, &mem, message);
    super::super::heap::host_retain(mem.data_mut(&mut *caller), request_val);
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

fn write_cors_preflight(stream: TcpStream) {
    let resp = "HTTP/1.1 204 No Content\r\n\
        Access-Control-Allow-Origin: *\r\n\
        Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
        Access-Control-Allow-Headers: Content-Type\r\n\
        Access-Control-Max-Age: 86400\r\n\
        Content-Length: 0\r\n\
        Connection: close\r\n\r\n";
    finish_response(stream, resp.as_bytes());
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
fn write_http_response(caller: &mut Caller<'_, ()>, stream: TcpStream, response_val: i64) {
    let val = response_val as u64;
    // Must be an object pointer.
    if (val & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        finish_response(
            stream,
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
    finish_response(stream, response.as_bytes());
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
    let tag = i32::from_le_bytes(
        data[inner_addr..inner_addr + 4]
            .try_into()
            .unwrap_or([0; 4]),
    );
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

fn read_array_item(
    mem: &Memory,
    caller: &mut Caller<'_, ()>,
    addr: usize,
    i: usize,
) -> Option<i64> {
    let data = mem.data(&*caller);
    let off = addr + 8 + i * 8;
    if off + 8 > data.len() {
        return None;
    }
    Some(i64::from_le_bytes(data[off..off + 8].try_into().ok()?))
}

fn read_dict_bool(
    mem: &Memory,
    caller: &mut Caller<'_, ()>,
    addr: usize,
    key: &str,
) -> Option<bool> {
    let val = dict_lookup(mem, caller, addr, key)?;
    let v = val as u64;
    if (v & (QNAN | SIGN_BIT | 0x0007_0000_0000_0000))
        == (QNAN | crate::wasm_runner::nan_box::TAG_BOOL)
    {
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
    // Async handler (a resume fn — `frame_size > 0`): can't be `call_indirect`'d
    // like a sync `FaiFunc`. Hand it to the guest scheduler's host-driver, which
    // spawns it as a task, drives `poll` to completion, and returns the result.
    if header.frame_size > 0 {
        let drive = caller
            .get_export("__fai_drive_closure")
            .ok_or_else(|| "async handler requires __fai_drive_closure".to_string())?
            .into_func()
            .ok_or_else(|| "__fai_drive_closure is not a func".to_string())?;
        let mut results = vec![Val::I64(0)];
        drive
            .call(
                &mut *caller,
                &[Val::I64(handler_val), Val::I64(arg)],
                &mut results,
            )
            .map_err(|e| format!("wasm trap: {}", e))?;
        return match results[0] {
            Val::I64(v) => Ok(v),
            _ => Err("unexpected result type".to_string()),
        };
    }
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

    // ── finish_response: graceful shutdown ──────────────────────────
    //
    // Regression coverage for the Content-Length truncation bug seen
    // through Fly's edge proxy: the response would arrive with
    // Content-Length: N but a body shorter than N bytes, because
    // dropping the TcpStream right after write_all called close(2)
    // before the kernel had drained the send buffer. The fix is
    // set_linger + explicit shutdown(Write); these tests pin that
    // contract on a real loopback socket.

    use std::net::TcpListener;
    use std::thread;

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        let client = TcpStream::connect(addr).expect("connect loopback");
        let (server, _peer) = listener.accept().expect("accept");
        (server, client)
    }

    #[test]
    fn finish_response_delivers_full_body() {
        let (server, mut client) = loopback_pair();
        // Larger than a single send-buffer chunk to exercise the
        // partial-write window where the truncation bug used to land.
        let payload = vec![b'x'; 256 * 1024];
        let writer = thread::spawn({
            let payload = payload.clone();
            move || finish_response(server, &payload)
        });
        let mut received = Vec::new();
        client.read_to_end(&mut received).expect("read_to_end");
        writer.join().expect("writer thread");
        assert_eq!(received.len(), payload.len());
        assert_eq!(received, payload);
    }

    #[test]
    fn finish_response_signals_eof_to_peer() {
        // shutdown(Write) is what lets the peer's read_to_end return
        // cleanly — without it some proxies wait until close() then
        // race the FIN against the buffered payload. Assert that
        // after finish_response returns, a subsequent read on the
        // peer sees EOF (0 bytes) rather than blocking.
        let (server, mut client) = loopback_pair();
        finish_response(server, b"hello");

        let mut buf = Vec::new();
        client.read_to_end(&mut buf).expect("read_to_end");
        assert_eq!(buf, b"hello");

        // A second read after EOF should immediately return 0 bytes
        // (Read::read on a closed half-stream).
        let mut tail = [0u8; 4];
        let n = client.read(&mut tail).expect("post-eof read");
        assert_eq!(n, 0);
    }

    #[test]
    fn drain_request_consumes_headers_so_close_sends_fin_not_rst() {
        // The root truncation bug: static-file responses never read
        // the request bytes, so on Linux close(2) saw unread data in
        // the recv buffer and sent RST instead of FIN. Fly's edge
        // proxy treats RST as "abort the stream" and stops forwarding
        // bytes to the client mid-body. Drain the request first and
        // close becomes graceful.
        let (server, mut client) = loopback_pair();

        // Simulate Fly's proxy: send a normal HTTP request then read
        // the response back. If the server's close sends RST instead
        // of FIN, read_to_end on the client side will surface
        // ECONNRESET via Err(...) rather than the full body.
        client
            .write_all(
                b"GET /fai-runtime.js HTTP/1.1\r\n\
                  Host: forailang.com\r\n\
                  User-Agent: probe\r\n\r\n",
            )
            .expect("write request");

        drain_request(&server);
        // Imitate the static-file path: write a large body then
        // shutdown(Write).
        let payload = vec![b'A'; 64 * 1024];
        finish_response(server, &payload);

        let mut received = Vec::new();
        client.read_to_end(&mut received).expect("read_to_end");
        assert_eq!(received.len(), payload.len());
    }

    #[test]
    fn drain_request_handles_post_body() {
        let (server, mut client) = loopback_pair();
        client
            .write_all(
                b"POST /upload HTTP/1.1\r\n\
                  Host: x\r\n\
                  Content-Length: 11\r\n\r\n\
                  hello world",
            )
            .expect("write request");
        drain_request(&server);
        finish_response(server, b"ok");
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).expect("read_to_end");
        assert_eq!(buf, b"ok");
    }

    #[test]
    fn finish_response_handles_empty_body() {
        // Preflight / 204 paths pass an empty body — make sure the
        // shutdown sequence still works and the peer sees a clean EOF.
        let (server, mut client) = loopback_pair();
        finish_response(server, b"");
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).expect("read_to_end");
        assert!(buf.is_empty());
    }
}
