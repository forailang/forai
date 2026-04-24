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

use wasmtime::*;

use super::super::heap::{build_value, wasm_alloc_str};
use super::super::nan_box::VAL_NULL;
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
