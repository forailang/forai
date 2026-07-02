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
    // Builds the full `HttpResponse` shape on the guest heap and returns a
    // NaN-boxed Dict pointer. Optional fields are present as null so typed
    // field reads that inline by declaration slot still match dictionary reads.
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
                let mut pending_connections: Vec<PendingConn> = Vec::new();
                let listener_raw_fd = {
                    use std::os::fd::AsRawFd;
                    listener.as_raw_fd()
                };
                let mut pending = PendingRequests::default();
                // Request reads submitted to the boundary but not yet handed
                // back by take_server_reads (plan 103 U3).
                let mut in_flight_reads: usize = 0;
                let scheduler = super::guest_scheduler::GuestScheduler::new(&mut caller);
                let _ = listener.set_nonblocking(true);
                // Test-only: count guest-scheduler polls so a regression test can
                // assert the loop parks instead of busy-polling while a handler
                // awaits a long call. Reported on loop exit when the env var is set.
                let count_polls = std::env::var("FAI_DEBUG_SERVER_POLLS").is_ok();
                let mut poll_count: u64 = 0;
                let trace_loop = std::env::var("FAI_TRACE_LOOP").is_ok();
                let loop_t0 = std::time::Instant::now();
                macro_rules! trace {
                    ($($arg:tt)*) => {
                        if trace_loop {
                            eprintln!("[loop {:>8.1}ms] {}", loop_t0.elapsed().as_secs_f64()*1000.0, format!($($arg)*));
                        }
                    };
                }
                // Wake source for new connections (plan 103 U5): a one-shot
                // readable watch on the listener, re-armed after each fire,
                // so a parked loop wakes the instant a client connects
                // instead of at the 250ms backstop.
                let mut listener_watch: Option<u64> = None;
                loop {
                    let accepting = max_requests.map_or(true, |m| accepted < m);

                    // 1. Drain every connection ready right now (while accepting).
                    if accepting {
                        loop {
                            match listener.accept() {
                                Ok((stream, _)) => {
                                    trace!("accepted connection");
                                    pending_connections.push(PendingConn {
                                        stream,
                                        watch: None,
                                    });
                                    accepted += 1;
                                }
                                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(_) => break,
                            }
                        }
                    }
                    let ifr_before = in_flight_reads;
                    process_ready_connections(&mut pending_connections, &mut in_flight_reads);
                    if in_flight_reads != ifr_before { trace!("submitted read job (in_flight={})", in_flight_reads); }

                    // 2. Request cap reached and all in-flight work drained →
                    // exit the server loop so the program can terminate.
                    if !accepting
                        && pending_connections.is_empty()
                        && pending.is_empty()
                        && in_flight_reads == 0
                    {
                        if count_polls {
                            output::stderr_line(&format!("__server_polls={}", poll_count));
                        }
                        if let Some(watch) = listener_watch.take() {
                            super::reactor::unwatch(watch);
                        }
                        break;
                    }

                    // 4. Advance in-flight handler tasks: run ready ones, resume
                    // any whose offloaded boundary work finished (outbound RPC,
                    // etc.), then write responses for tasks that completed.
                    for task_id in super::boundary::pump_ready() {
                        scheduler.resume_task(&mut caller, task_id);
                    }
                    // Fired reactor watches: stdlib socket waits first (they
                    // resume guest tasks); what remains belongs to this loop —
                    // the listener watch or a pending connection's watch. A
                    // fired watch is consumed (one-shot), so clear it here and
                    // re-arm below / next pass.
                    let mut fired_watches = super::boundary::take_readiness();
                    for task_id in super::sockets::handle_ready_watches(&mut fired_watches) {
                        scheduler.resume_task(&mut caller, task_id);
                    }
                    for watch_id in fired_watches {
                        if listener_watch == Some(watch_id) {
                            listener_watch = None;
                        } else if let Some(conn) = pending_connections
                            .iter_mut()
                            .find(|c| c.watch == Some(watch_id))
                        {
                            conn.watch = None;
                        }
                    }
                    // Requests whose off-thread read finished (routed out of
                    // the completion queue by the pump_ready above): build the
                    // guest request and dispatch (spawn or inline). Must drain
                    // after pump_ready and before the park — a read left in
                    // the thread-local queue would not re-signal the condvar,
                    // stalling the request until the timer backstop.
                    for read in super::boundary::take_server_reads() {
                        in_flight_reads = in_flight_reads.saturating_sub(1);
                        trace!("handling completed read");
                        if let Ok(boxed) = read {
                            if let Ok(done) = boxed.downcast::<ServerReadDone>() {
                                handle_read_request(
                                    &mut caller,
                                    id as u32,
                                    &scheduler,
                                    *done,
                                    &mut pending,
                                );
                            }
                        }
                    }
                    let _ = scheduler.poll(&mut caller);
                    poll_count += 1;
                    super::async_ops::prune_fired_timers();
                    finish_completed(&mut caller, &scheduler, &mut pending);

                    // Driver diagnostic (FAI_DEBUG_SERVER_POLLS): every 100 polls,
                    // report the guest live-task count + pending requests, plus a
                    // task-status histogram when live_count is high. The poll_count
                    // cadence itself is the signal — at the timer-aware park rate an
                    // idle server emits these ~every 25s, not every 2.5s. A climbing
                    // live_count that never returns to baseline would mean completed
                    // tasks are leaking and the loop can't reach the blocking accept.
                    if count_polls && poll_count % 100 == 0 {
                        let lc = scheduler.live_count(&mut caller);
                        let mut ready = 0;
                        let mut running = 0;
                        let mut waiting = 0;
                        let mut complete = 0;
                        if lc > 8 {
                            let mut id = 0;
                            while id < 512 {
                                match scheduler.task_status(&mut caller, id) {
                                    0 => ready += 1,
                                    1 => running += 1,
                                    2 => waiting += 1,
                                    3 => complete += 1,
                                    _ => {}
                                }
                                id += 1;
                            }
                        }
                        output::stderr_line(&format!(
                            "__driver poll_count={} live_count={} pending={} ready={} running={} waiting={} complete={}",
                            poll_count, lc, pending.len(), ready, running, waiting, complete,
                        ));
                    }

                    // 5. Park until the next real event — a boundary completion
                    // (condvar), a fired reactor watch (new connection, request
                    // bytes on a pending connection, a stdlib socket wait), or
                    // the nearest pending sleep-timer. `__fai_poll` already ran
                    // the ready queue to quiescence, so nothing is runnable
                    // right now. Every external wake source is watch- or
                    // condvar-driven (plan 103 U5), so there is no polling
                    // branch left — the backstop only bounds the park.
                    if accepting && listener_watch.is_none() {
                        listener_watch =
                            Some(super::reactor::watch_readable(listener_raw_fd));
                    }
                    trace!(
                        "park (pending={} reads={} conns={})",
                        pending.len(),
                        in_flight_reads,
                        pending_connections.len()
                    );
                    park_until_next_event();
                    trace!("park wake");
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

#[derive(Default)]
struct PendingRequests {
    entries: Vec<PendingRequest>,
    by_task: HashMap<i32, usize>,
}

impl PendingRequests {
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn task_id(&self, index: usize) -> i32 {
        self.entries[index].task_id
    }

    fn push(&mut self, request: PendingRequest) {
        self.by_task.insert(request.task_id, self.entries.len());
        self.entries.push(request);
    }

    fn remove_task(&mut self, task_id: i32) -> Option<PendingRequest> {
        let index = *self.by_task.get(&task_id)?;
        Some(self.remove_index(index))
    }

    fn remove_index(&mut self, index: usize) -> PendingRequest {
        let removed = self.entries.swap_remove(index);
        self.by_task.remove(&removed.task_id);
        if index < self.entries.len() {
            self.by_task.insert(self.entries[index].task_id, index);
        }
        removed
    }
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
/// An accepted connection whose request bytes haven't arrived yet, plus its
/// reactor watch (armed while waiting so the parked loop wakes on first
/// bytes instead of polling; plan 103 U5).
struct PendingConn {
    stream: TcpStream,
    watch: Option<u64>,
}

/// Move every connection with request bytes available into an off-thread
/// request read (plan 103 U3). Connections still waiting for bytes get a
/// readable watch armed (plan 103 U5). The read job returns the full request
/// as owned data via `boundary::take_server_reads`; `handle_read_request`
/// finishes the dispatch on the main thread. `in_flight_reads` counts
/// submitted-but-not-yet-handled reads so the loop's exit/idle conditions
/// don't strand one.
fn process_ready_connections(
    pending_connections: &mut Vec<PendingConn>,
    in_flight_reads: &mut usize,
) {
    let mut i = 0;
    while i < pending_connections.len() {
        match connection_readiness(&pending_connections[i].stream) {
            ConnectionReadiness::Ready => {
                let conn = pending_connections.swap_remove(i);
                if let Some(watch) = conn.watch {
                    super::reactor::unwatch(watch);
                }
                submit_request_read(conn.stream, in_flight_reads);
            }
            ConnectionReadiness::Pending => {
                if pending_connections[i].watch.is_none() {
                    use std::os::fd::AsRawFd;
                    let fd = pending_connections[i].stream.as_raw_fd();
                    pending_connections[i].watch =
                        Some(super::reactor::watch_readable(fd));
                }
                i += 1;
            }
            ConnectionReadiness::Closed => {
                let conn = pending_connections.swap_remove(i);
                if let Some(watch) = conn.watch {
                    super::reactor::unwatch(watch);
                }
            }
        }
    }
}

/// Read the request off-thread. A slow client (drip-fed request line, stalled
/// body) now costs a waiter thread up to the 5s read timeout instead of
/// stalling every other connection on the scheduler thread.
fn submit_request_read(stream: TcpStream, in_flight_reads: &mut usize) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    *in_flight_reads += 1;
    super::boundary::submit_server_read(move || {
        let request = read_request_owned(&stream);
        ServerReadDone { stream, request }
    });
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

/// Finish one request whose bytes arrived from an off-thread read (plan 103
/// U3). OPTIONS preflight and static files are answered from the owned data;
/// otherwise the request is materialized on the guest heap and dispatched —
/// an async handler is spawned as a scheduler task (its response written
/// later, when the task completes), while a sync handler or a 404 resolves
/// inline exactly as before.
fn handle_read_request(
    caller: &mut Caller<'_, ()>,
    router_id: u32,
    scheduler: &super::guest_scheduler::GuestScheduler,
    done: ServerReadDone,
    pending: &mut PendingRequests,
) {
    let ServerReadDone { stream, request } = done;
    let Some(req) = request else {
        // Closed / timed out / malformed: nothing to answer.
        return;
    };
    if req.method == "OPTIONS" {
        write_cors_preflight(stream);
        return;
    }
    // Static files first (binary-safe direct serving); else the WASM handler.
    if req.method == "GET" {
        if let Some(static_response) = try_serve_static_from_router(router_id, &req.path) {
            write_raw_response(stream, static_response);
            return;
        }
    }
    let request_val = {
        let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
        build_request_into_guest(caller, &mem, &req)
    };
    super::events::dispatch_event(caller, "http:beforeRequest", request_val);

    // Async handler → spawn as a task and defer its response. Sync handlers,
    // 404s, and handler errors all resolve inline via dispatch_router_request.
    if let Some(handler) = find_matching_handler(caller, router_id, request_val) {
        if handler_is_async(caller, handler) {
            if let Some(task_id) = scheduler.spawn_queued_closure(caller, handler, request_val) {
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
fn finish_completed(
    caller: &mut Caller<'_, ()>,
    scheduler: &super::guest_scheduler::GuestScheduler,
    pending: &mut PendingRequests,
) {
    if scheduler.has_completed_queue() {
        while let Some(task_id) = scheduler.pop_completed_task(caller) {
            let _ = finish_completed_task(caller, scheduler, pending, task_id);
        }
        return;
    }

    let mut i = 0;
    while i < pending.len() {
        let status = scheduler.task_status(caller, pending.task_id(i));
        if status >= ST_COMPLETE {
            finish_completed_at(caller, scheduler, pending, i, status);
        } else {
            i += 1;
        }
    }
}

fn finish_completed_task(
    caller: &mut Caller<'_, ()>,
    scheduler: &super::guest_scheduler::GuestScheduler,
    pending: &mut PendingRequests,
    task_id: i32,
) -> bool {
    let Some(p) = pending.remove_task(task_id) else {
        return false;
    };
    let status = scheduler.task_status(caller, task_id);
    if status < ST_COMPLETE {
        pending.push(p);
        return false;
    }
    finish_completed_request(caller, scheduler, p, status);
    true
}

fn finish_completed_at(
    caller: &mut Caller<'_, ()>,
    scheduler: &super::guest_scheduler::GuestScheduler,
    pending: &mut PendingRequests,
    index: usize,
    status: i32,
) {
    let p = pending.remove_index(index);
    finish_completed_request(caller, scheduler, p, status);
}

fn finish_completed_request(
    caller: &mut Caller<'_, ()>,
    scheduler: &super::guest_scheduler::GuestScheduler,
    p: PendingRequest,
    status: i32,
) {
    let response = if status == ST_FAILED {
        let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
        let error_val = scheduler.task_result(caller, p.task_id);
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
        scheduler.task_result(caller, p.task_id)
    };
    complete_request(caller, p.stream, p.request_val, response);
    // Slot was marked host-driven, so reclaim it ourselves now that we have
    // read the result (mirrors __fai_drive_closure's inline free).
    scheduler.free_task(caller, p.task_id);
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

// Park the driver until its next real event: a boundary completion (an outbound
// call or FFI offload — the condvar wakes the instant it finishes), the nearest
// pending sleep-timer deadline, or the backstop cap, whichever comes first.
// `__fai_poll` runs the guest ready queue to quiescence before returning, so once
// a poll returns there is no runnable task left to advance — every live task is
// parked on a timer or a host op. Re-polling before the next tracked event would
// only burn CPU: the old fixed 1ms/25ms re-poll pegged a core, and on a server
// whose background `nowait` loops keep live_count>1 (e.g. the brain server) it
// spun ~40x/sec even while fully idle, because the no-inflight path slept a fixed
// 25ms instead of honoring the timer. `next_poll_timeout` returns the nearest
// pending timer clamped to [1ms, 250ms], so even with no boundary work in flight
// the loop sleeps until that deadline rather than a fixed fine cadence.
fn park_until_next_event() {
    super::async_ops::park_for_next_event();
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

/// Write a raw byte response to the TCP stream (off the scheduler thread).
fn write_raw_response(stream: TcpStream, response: Vec<u8>) {
    send_response(stream, response);
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
    let key_content_type = wasm_alloc_str(caller, mem, "contentType");
    let key_location = wasm_alloc_str(caller, mem, "location");
    let key_cookies = wasm_alloc_str(caller, mem, "cookies");
    let key_headers = wasm_alloc_str(caller, mem, "headers");

    let body_val = wasm_alloc_str(caller, mem, body);
    let status_val = (QNAN | TAG_INT | (status as u32 as u64)) as i64;

    let content_type_val = match kind {
        KIND_TEXT => wasm_alloc_str(caller, mem, "text/plain"),
        KIND_HTML => wasm_alloc_str(caller, mem, "text/html; charset=utf-8"),
        _ => VAL_NULL,
    };
    // For redirect the `body` arg is actually the URL. The VM still sets body
    // to "", so `location` keeps the supplied value and `body` is replaced.
    let location_val = if kind == KIND_REDIRECT {
        body_val
    } else {
        VAL_NULL
    };

    // For redirect, body is empty.
    let body_val_final = if kind == KIND_REDIRECT {
        wasm_alloc_str(caller, mem, "")
    } else {
        body_val
    };

    alloc_dict(
        caller,
        mem,
        &[
            (key_status, status_val),
            (key_body, body_val_final),
            (key_content_type, content_type_val),
            (key_location, location_val),
            (key_cookies, VAL_NULL),
            (key_headers, VAL_NULL),
        ],
    )
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

fn write_cors_preflight(stream: TcpStream) {
    let resp = "HTTP/1.1 204 No Content\r\n\
        Access-Control-Allow-Origin: *\r\n\
        Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
        Access-Control-Allow-Headers: Content-Type\r\n\
        Access-Control-Max-Age: 86400\r\n\
        Content-Length: 0\r\n\
        Connection: close\r\n\r\n";
    send_response(stream, resp.as_bytes().to_vec());
}

/// A fully read request as owned data — produced on a boundary waiter thread
/// (plan 103 U3), consumed on the main thread by `build_request_into_guest`.
/// Reading off-thread means a slow client (drip-fed headers, a stalled body)
/// costs a waiter, never the scheduler.
pub(crate) struct OwnedRequest {
    method: String,
    path: String,
    query_string: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// A finished server-side request read, handed back to the server loop via
/// `boundary::take_server_reads`. `request` is None for a connection that
/// closed, timed out, or sent a malformed request line.
struct ServerReadDone {
    stream: TcpStream,
    request: Option<OwnedRequest>,
}

/// Read one full request (request line + headers + Content-Length body) into
/// owned data. Pure socket I/O and parsing — no `Store`, no guest memory —
/// so it can run on a waiter thread. Bounded by the stream's read timeout
/// (set by the caller) per read call.
fn read_request_owned(stream: &TcpStream) -> Option<OwnedRequest> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return None;
    }
    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return None;
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
    let body = String::from_utf8_lossy(&body_bytes).into_owned();
    Some(OwnedRequest {
        method,
        path,
        query_string,
        headers,
        body,
    })
}

/// Build the `{method, path, body, headers, query}` Dict on the guest heap
/// from an owned request. Main thread only (touches guest memory). Mirrors
/// VM's `parse_http_request`.
fn build_request_into_guest(
    caller: &mut Caller<'_, ()>,
    mem: &Memory,
    req: &OwnedRequest,
) -> i64 {
    let OwnedRequest {
        method,
        path,
        query_string,
        headers,
        body: body_str,
    } = req;

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
        send_response(
            stream,
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
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
    send_response(stream, response.into_bytes());
}

/// Write `response` to the client from a boundary waiter thread (plan 103
/// U3): the bytes are fully owned by now, so a slow-to-read client costs a
/// waiter, never the scheduler. `finish_response` keeps the graceful-FIN
/// shutdown semantics.
fn send_response(stream: TcpStream, response: Vec<u8>) {
    super::boundary::submit_detached(move || finish_response(stream, &response));
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
    fn read_request_consumes_headers_so_close_sends_fin_not_rst() {
        // The root truncation bug: static-file responses never read
        // the request bytes, so on Linux close(2) saw unread data in
        // the recv buffer and sent RST instead of FIN. Fly's edge
        // proxy treats RST as "abort the stream" and stops forwarding
        // bytes to the client mid-body. `read_request_owned` consumes
        // the full request (plan 103 U3), so close stays graceful.
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

        let req = read_request_owned(&server).expect("parse request");
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/fai-runtime.js");
        // Imitate the static-file path: write a large body then
        // shutdown(Write).
        let payload = vec![b'A'; 64 * 1024];
        finish_response(server, &payload);

        let mut received = Vec::new();
        client.read_to_end(&mut received).expect("read_to_end");
        assert_eq!(received.len(), payload.len());
    }

    #[test]
    fn read_request_consumes_post_body() {
        let (server, mut client) = loopback_pair();
        client
            .write_all(
                b"POST /upload HTTP/1.1\r\n\
                  Host: x\r\n\
                  Content-Length: 11\r\n\r\n\
                  hello world",
            )
            .expect("write request");
        let req = read_request_owned(&server).expect("parse request");
        assert_eq!(req.method, "POST");
        assert_eq!(req.body, "hello world");
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
