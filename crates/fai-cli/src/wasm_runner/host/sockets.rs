//! TCP + UDP host imports. Each is a thin marshalling layer over
//! `socket_registry` which owns the underlying `TcpListener`,
//! `TcpStream`, and `UdpSocket` objects across calls.
//!
//! Error semantics (VM parity, mapped to wasm-friendly returns):
//!
//! - Functions that return a handle (`listen`/`accept`/`connect`/`bind`)
//!   return `-1` on error.
//! - Functions that return a String (`read`/`readLine`/`address`) return
//!   `VAL_NULL` on error.
//! - Functions that return a Dict (`accept`, `udp.receive`) return
//!   `VAL_NULL` on error.
//! - `close`/`broadcast` return void; errors are swallowed.
//!
//! `native_tcp_*` / `native_udp_*` in fai-runtime raise typed errors
//! instead; that divergence is documented in plans/93-audit-natives.md.

use std::net::TcpStream;

use wasmtime::*;

use super::super::heap::{build_value, wasm_alloc_str};
use super::super::nan_box::VAL_NULL;
use super::host_ops::{read_int_value, read_string_arg, submit_host_op, submit_host_wait, HostOpResult};
use super::socket_registry as reg;

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // ── TCP ──────────────────────────────────────────────────────────

    linker
        .func_wrap(
            "env",
            "tcp_listen",
            |_c: Caller<'_, ()>, port: i32| -> i32 {
                reg::tcp_listen(port as u16).map(|h| h as i32).unwrap_or(-1)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "tcp_accept",
            |mut caller: Caller<'_, ()>, handle: i32| -> i64 {
                match reg::tcp_accept(handle as u32) {
                    Ok((conn_id, address)) => {
                        let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                            Some(m) => m,
                            None => return VAL_NULL,
                        };
                        let val = serde_json::json!({
                            "handle": conn_id as i64,
                            "address": address,
                        });
                        build_value(&mut caller, &mem, &val)
                    }
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "tcp_connect",
            |mut caller: Caller<'_, ()>, host_ptr: i32, host_len: i32, port: i32| -> i32 {
                let host = read_guest_str(&mut caller, host_ptr, host_len);
                reg::tcp_connect(&host, port as u16)
                    .map(|h| h as i32)
                    .unwrap_or(-1)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "tcp_read",
            |mut caller: Caller<'_, ()>, handle: i32| -> i64 {
                let result = reg::tcp_read(handle as u32);
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return VAL_NULL,
                };
                match result {
                    Ok(s) => wasm_alloc_str(&mut caller, &mem, &s),
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "tcp_read_line",
            |mut caller: Caller<'_, ()>, handle: i32| -> i64 {
                let result = reg::tcp_read_line(handle as u32);
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return VAL_NULL,
                };
                match result {
                    Ok(s) => wasm_alloc_str(&mut caller, &mem, &s),
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "tcp_write",
            |mut caller: Caller<'_, ()>, handle: i32, ptr: i32, len: i32| -> i32 {
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return -1,
                };
                let data = {
                    let d = mem.data(&caller);
                    let end = (ptr as usize).saturating_add(len as usize);
                    if end > d.len() {
                        return -1;
                    }
                    d[ptr as usize..end].to_vec()
                };
                reg::tcp_write(handle as u32, &data)
                    .map(|n| n as i32)
                    .unwrap_or(-1)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap("env", "tcp_close", |_c: Caller<'_, ()>, handle: i32| {
            // Complete any readiness-parked wait on this handle with Null
            // before the fd disappears — epoll drops a closed fd silently,
            // so without this the parked task would never resume.
            cancel_reactor_waits(handle as u32);
            let _ = reg::socket_close(handle as u32);
        })
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "tcp_address",
            |mut caller: Caller<'_, ()>, handle: i32| -> i64 {
                let result = reg::tcp_address(handle as u32);
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return VAL_NULL,
                };
                match result {
                    Ok(s) => wasm_alloc_str(&mut caller, &mem, &s),
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // ── UDP ──────────────────────────────────────────────────────────

    linker
        .func_wrap("env", "udp_bind", |_c: Caller<'_, ()>, port: i32| -> i32 {
            reg::udp_bind(port as u16).map(|h| h as i32).unwrap_or(-1)
        })
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "udp_send",
            |mut caller: Caller<'_, ()>,
             handle: i32,
             host_ptr: i32,
             host_len: i32,
             port: i32,
             data_ptr: i32,
             data_len: i32|
             -> i32 {
                let host = read_guest_str(&mut caller, host_ptr, host_len);
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return -1,
                };
                let data = {
                    let d = mem.data(&caller);
                    let end = (data_ptr as usize).saturating_add(data_len as usize);
                    if end > d.len() {
                        return -1;
                    }
                    d[data_ptr as usize..end].to_vec()
                };
                reg::udp_send(handle as u32, &host, port as u16, &data)
                    .map(|n| n as i32)
                    .unwrap_or(-1)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "udp_receive",
            |mut caller: Caller<'_, ()>, handle: i32| -> i64 {
                match reg::udp_receive(handle as u32) {
                    Ok((data, host, port)) => {
                        let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                            Some(m) => m,
                            None => return VAL_NULL,
                        };
                        let data_str = String::from_utf8_lossy(&data).into_owned();
                        let val = serde_json::json!({
                            "data": data_str,
                            "host": host,
                            "port": port as i64,
                        });
                        build_value(&mut caller, &mem, &val)
                    }
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "udp_broadcast",
            |_c: Caller<'_, ()>, handle: i32, enabled: i32| {
                let _ = reg::udp_set_broadcast(handle as u32, enabled != 0);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

pub(super) fn begin_socket_host_op(
    caller: &mut Caller<'_, ()>,
    task_id: i32,
    op_kind: i32,
    args: &[i64],
) -> bool {
    match op_kind {
        fai_codegen_wasm::HOST_OP_TCP_ACCEPT => {
            let Some(handle) = read_int_arg(args, 0) else {
                submit_socket_null(task_id);
                return true;
            };
            begin_reactor_wait(task_id, handle as u32, ReactorWaitKind::Accept);
            true
        }
        fai_codegen_wasm::HOST_OP_TCP_CONNECT => {
            let Some(host) = read_string_arg(caller, args, 0) else {
                submit_socket_int(task_id, -1);
                return true;
            };
            let Some(port) = read_int_arg(args, 1) else {
                submit_socket_int(task_id, -1);
                return true;
            };
            // Connect stays a boundary Wait: it is a one-shot blocking call
            // (DNS + handshake) rather than an indefinite readable-wait, and
            // the reactor only reports readability.
            submit_host_wait(task_id, move || {
                let addr = format!("{}:{}", host, port as u16);
                match TcpStream::connect(&addr) {
                    Ok(stream) => HostOpResult::TcpConnected(stream),
                    Err(_) => HostOpResult::Int(-1),
                }
            });
            true
        }
        fai_codegen_wasm::HOST_OP_TCP_READ => {
            let Some(handle) = read_int_arg(args, 0) else {
                submit_socket_null(task_id);
                return true;
            };
            begin_reactor_wait(task_id, handle as u32, ReactorWaitKind::Read);
            true
        }
        fai_codegen_wasm::HOST_OP_TCP_READ_LINE => {
            let Some(handle) = read_int_arg(args, 0) else {
                submit_socket_null(task_id);
                return true;
            };
            begin_reactor_wait(
                task_id,
                handle as u32,
                ReactorWaitKind::ReadLine(Vec::new()),
            );
            true
        }
        fai_codegen_wasm::HOST_OP_UDP_RECEIVE => {
            let Some(handle) = read_int_arg(args, 0) else {
                submit_socket_null(task_id);
                return true;
            };
            begin_reactor_wait(task_id, handle as u32, ReactorWaitKind::UdpReceive);
            true
        }
        _ => false,
    }
}

// ── Readiness-driven socket waits (plan 103 U5) ──────────────────────────
//
// A parked socket op no longer occupies a waiter thread in a poll loop.
// `begin_reactor_wait` first attempts the non-blocking op inline (data may
// already be buffered); on WouldBlock it registers a one-shot readable watch
// with the reactor and parks the task. The driver loop hands fired watch ids
// to `handle_ready_watches`, which performs the I/O on the main thread and
// either completes the parked task (via `boundary::insert_ready`) or re-arms
// the watch. `tcp.close` cancels pending waits (`cancel_reactor_waits`) —
// epoll forgets closed fds silently, so close must resolve them to Null.

enum ReactorWaitKind {
    Accept,
    Read,
    ReadLine(Vec<u8>),
    UdpReceive,
}

struct ReactorWait {
    task_id: i32,
    handle: u32,
    kind: ReactorWaitKind,
}

thread_local! {
    /// Parked socket waits keyed by their reactor watch id.
    static REACTOR_WAITS: std::cell::RefCell<std::collections::HashMap<u64, ReactorWait>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Tasks completed outside a readiness event (close-cancel, inline
    /// completion) that still need `__fai_resume_task`.
    static PENDING_RESUMES: std::cell::RefCell<Vec<i32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// One attempt at a wait's non-blocking op.
enum WaitProgress {
    Done(HostOpResult),
    NotReady,
}

fn attempt(wait: &mut ReactorWait) -> WaitProgress {
    match &mut wait.kind {
        ReactorWaitKind::Accept => match reg::try_tcp_accept(wait.handle) {
            Ok(Some((stream, address))) => {
                WaitProgress::Done(HostOpResult::TcpAccepted { stream, address })
            }
            Ok(None) => WaitProgress::NotReady,
            Err(_) => WaitProgress::Done(HostOpResult::Null),
        },
        ReactorWaitKind::Read => match reg::try_tcp_read(wait.handle) {
            Ok(Some(data)) => WaitProgress::Done(HostOpResult::String(data)),
            Ok(None) => WaitProgress::NotReady,
            Err(_) => WaitProgress::Done(HostOpResult::Null),
        },
        ReactorWaitKind::ReadLine(partial) => match reg::try_tcp_read_line(wait.handle, partial) {
            reg::TryLineStep::Line(line) => WaitProgress::Done(HostOpResult::String(line)),
            reg::TryLineStep::Pending => WaitProgress::NotReady,
            reg::TryLineStep::Failed(_) => WaitProgress::Done(HostOpResult::Null),
        },
        ReactorWaitKind::UdpReceive => match reg::try_udp_receive(wait.handle) {
            Ok(Some((data, host, port))) => {
                let data_str = String::from_utf8_lossy(&data).into_owned();
                WaitProgress::Done(HostOpResult::Json(serde_json::json!({
                    "data": data_str,
                    "host": host,
                    "port": port as i64,
                })))
            }
            Ok(None) => WaitProgress::NotReady,
            Err(_) => WaitProgress::Done(HostOpResult::Null),
        },
    }
}

fn complete_wait(task_id: i32, result: HostOpResult) {
    super::boundary::insert_ready(task_id, Ok(Box::new(result)));
    PENDING_RESUMES.with(|q| q.borrow_mut().push(task_id));
}

fn begin_reactor_wait(task_id: i32, handle: u32, kind: ReactorWaitKind) {
    let mut wait = ReactorWait {
        task_id,
        handle,
        kind,
    };
    // Fast path: the data (or connection) may already be waiting.
    if let WaitProgress::Done(result) = attempt(&mut wait) {
        complete_wait(task_id, result);
        return;
    }
    arm_wait(wait);
}

fn arm_wait(wait: ReactorWait) {
    let fd = match reg::socket_raw_fd(wait.handle) {
        Ok(fd) => fd,
        Err(_) => {
            complete_wait(wait.task_id, HostOpResult::Null);
            return;
        }
    };
    let watch_id = super::reactor::watch_readable(fd);
    REACTOR_WAITS.with(|w| {
        w.borrow_mut().insert(watch_id, wait);
    });
}

/// Driver hook: consume fired watch ids that belong to socket waits, perform
/// their I/O, and return the task ids now ready to resume (including waits
/// completed inline or cancelled by `tcp.close` since the last call). Ids
/// that belong to other subsystems (the HTTP server's connection watches)
/// are left in `ids`.
pub(crate) fn handle_ready_watches(ids: &mut Vec<u64>) -> Vec<i32> {
    let mut resumed: Vec<i32> = PENDING_RESUMES.with(|q| std::mem::take(&mut *q.borrow_mut()));
    ids.retain(|watch_id| {
        let Some(mut wait) = REACTOR_WAITS.with(|w| w.borrow_mut().remove(watch_id)) else {
            return true; // not a socket wait — leave for other consumers
        };
        match attempt(&mut wait) {
            WaitProgress::Done(result) => {
                super::boundary::insert_ready(wait.task_id, Ok(Box::new(result)));
                resumed.push(wait.task_id);
            }
            WaitProgress::NotReady => arm_wait(wait), // spurious/partial: re-arm
        }
        false
    });
    resumed
}

/// Resolve every parked wait on `handle` to Null (the socket is closing).
fn cancel_reactor_waits(handle: u32) {
    let cancelled: Vec<(u64, i32)> = REACTOR_WAITS.with(|w| {
        let mut map = w.borrow_mut();
        let ids: Vec<u64> = map
            .iter()
            .filter(|(_, wait)| wait.handle == handle)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .map(|id| {
                let wait = map.remove(&id).unwrap();
                (id, wait.task_id)
            })
            .collect()
    });
    for (watch_id, task_id) in cancelled {
        super::reactor::unwatch(watch_id);
        complete_wait(task_id, HostOpResult::Null);
    }
}

fn read_int_arg(args: &[i64], idx: usize) -> Option<i32> {
    read_int_value(*args.get(idx)?)
}

fn submit_socket_null(task_id: i32) {
    submit_host_op(task_id, || HostOpResult::Null);
}

fn submit_socket_int(task_id: i32, value: i32) {
    submit_host_op(task_id, move || HostOpResult::Int(value));
}







fn read_guest_str(caller: &mut Caller<'_, ()>, ptr: i32, len: i32) -> String {
    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return String::new(),
    };
    let data = mem.data(&*caller);
    let end = (ptr as usize).saturating_add(len as usize);
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[ptr as usize..end]).into_owned()
}
