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

use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use wasmtime::*;

use super::super::heap::{build_value, wasm_alloc_str};
use super::super::nan_box::VAL_NULL;
use super::host_ops::{read_int_value, read_string_arg, submit_host_op, HostOpResult};
use super::socket_registry as reg;

const SOCKET_WAIT_POLL: Duration = Duration::from_millis(20);

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
            match reg::clone_tcp_listener_for_wait(handle as u32) {
                Ok(wait) => {
                    submit_host_op(task_id, move || match wait_tcp_accept(wait) {
                        Some((stream, address)) => HostOpResult::TcpAccepted { stream, address },
                        None => HostOpResult::Null,
                    });
                }
                Err(_) => submit_socket_null(task_id),
            }
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
            submit_host_op(task_id, move || {
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
            match reg::clone_tcp_stream_for_wait(handle as u32) {
                Ok(wait) => {
                    submit_host_op(task_id, move || match wait_tcp_read(wait) {
                        Some(data) => HostOpResult::String(data),
                        None => HostOpResult::Null,
                    });
                }
                Err(_) => submit_socket_null(task_id),
            }
            true
        }
        fai_codegen_wasm::HOST_OP_TCP_READ_LINE => {
            let Some(handle) = read_int_arg(args, 0) else {
                submit_socket_null(task_id);
                return true;
            };
            match reg::clone_tcp_stream_for_wait(handle as u32) {
                Ok(wait) => {
                    submit_host_op(task_id, move || match wait_tcp_read_line(wait) {
                        Some(data) => HostOpResult::String(data),
                        None => HostOpResult::Null,
                    });
                }
                Err(_) => submit_socket_null(task_id),
            }
            true
        }
        fai_codegen_wasm::HOST_OP_UDP_RECEIVE => {
            let Some(handle) = read_int_arg(args, 0) else {
                submit_socket_null(task_id);
                return true;
            };
            match reg::clone_udp_socket_for_wait(handle as u32) {
                Ok(wait) => {
                    submit_host_op(task_id, move || match wait_udp_receive(wait) {
                        Some((data, host, port)) => {
                            let data_str = String::from_utf8_lossy(&data).into_owned();
                            HostOpResult::Json(serde_json::json!({
                                "data": data_str,
                                "host": host,
                                "port": port as i64,
                            }))
                        }
                        None => HostOpResult::Null,
                    });
                }
                Err(_) => submit_socket_null(task_id),
            }
            true
        }
        _ => false,
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

fn wait_tcp_accept(wait: reg::TcpListenerWait) -> Option<(TcpStream, String)> {
    if wait.listener.set_nonblocking(true).is_err() {
        return None;
    }
    loop {
        if is_cancelled(&wait.cancel) {
            return None;
        }
        match wait.listener.accept() {
            Ok((stream, addr)) => {
                if is_cancelled(&wait.cancel) {
                    return None;
                }
                return Some((stream, addr.to_string()));
            }
            Err(e) if retry_socket_wait(&e) => {
                thread::sleep(SOCKET_WAIT_POLL);
            }
            Err(_) => return None,
        }
    }
}

fn wait_tcp_read(mut wait: reg::TcpStreamWait) -> Option<String> {
    if wait
        .stream
        .set_read_timeout(Some(SOCKET_WAIT_POLL))
        .is_err()
    {
        return None;
    }
    let mut buf = [0u8; 8192];
    loop {
        if is_cancelled(&wait.cancel) {
            return None;
        }
        match wait.stream.read(&mut buf) {
            Ok(n) => {
                if is_cancelled(&wait.cancel) {
                    return None;
                }
                return Some(String::from_utf8_lossy(&buf[..n]).into_owned());
            }
            Err(e) if retry_socket_wait(&e) => {}
            Err(_) => return None,
        }
    }
}

fn wait_tcp_read_line(mut wait: reg::TcpStreamWait) -> Option<String> {
    if wait
        .stream
        .set_read_timeout(Some(SOCKET_WAIT_POLL))
        .is_err()
    {
        return None;
    }
    let mut line = Vec::new();
    loop {
        if is_cancelled(&wait.cancel) {
            return None;
        }
        let mut byte = [0u8; 1];
        match wait.stream.read(&mut byte) {
            Ok(0) => return Some(String::from_utf8_lossy(&line).into_owned()),
            Ok(_) => {
                if is_cancelled(&wait.cancel) {
                    return None;
                }
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    return Some(String::from_utf8_lossy(&line).into_owned());
                }
            }
            Err(e) if retry_socket_wait(&e) => {}
            Err(_) => return None,
        }
    }
}

fn wait_udp_receive(wait: reg::UdpSocketWait) -> Option<(Vec<u8>, String, u16)> {
    if wait
        .socket
        .set_read_timeout(Some(SOCKET_WAIT_POLL))
        .is_err()
    {
        return None;
    }
    let mut buf = vec![0u8; 65_535];
    loop {
        if is_cancelled(&wait.cancel) {
            return None;
        }
        match wait.socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                if is_cancelled(&wait.cancel) {
                    return None;
                }
                buf.truncate(n);
                return Some((buf, addr.ip().to_string(), addr.port()));
            }
            Err(e) if retry_socket_wait(&e) => {}
            Err(_) => return None,
        }
    }
}

fn retry_socket_wait(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    )
}

fn is_cancelled(cancel: &Arc<AtomicBool>) -> bool {
    cancel.load(Ordering::SeqCst)
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
