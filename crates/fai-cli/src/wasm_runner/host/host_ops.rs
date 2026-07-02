//! Generic async host-operation imports.
//!
//! These imports provide the shared begin/result ABI for stdlib operations that
//! need to offload blocking host work while keeping the guest scheduler
//! cooperative. Operation-specific modules copy their inputs into owned `Send`
//! data, submit work to the boundary, and marshal the owned result back here.

use serde_json::Value;
use std::net::TcpStream;
use wasmtime::{Caller, Linker, Memory};

use super::super::heap::{build_value, wasm_alloc_str};
use super::super::nan_box::{
    ADDR_MASK, OBJ_TAG_STRING, QNAN, SIGN_BIT, TAG_INT, TAG_MASK, VAL_NULL,
};
use super::events::{alloc_dict, write_global_i32, write_global_i64};

/// Owned result for a generic async host operation.
#[allow(dead_code)]
pub(crate) enum HostOpResult {
    RawI64(i64),
    Json(Value),
    Null,
    EnvLoad {
        ok: bool,
        pairs: Vec<(String, String)>,
    },
    Int(i32),
    String(String),
    TcpAccepted {
        stream: TcpStream,
        address: String,
    },
    TcpConnected(TcpStream),
    Error(String),
}

/// Submit generic host-operation *work* (short, resource-bound: file I/O,
/// instant error results) to the blocking boundary's bounded pool.
pub(crate) fn submit_host_op<F>(task_id: i32, work: F)
where
    F: FnOnce() -> HostOpResult + Send + 'static,
{
    submit_host_class(task_id, super::boundary::JobClass::Work, work)
}

/// Submit a generic host-operation *wait* (peer/child-paced, unbounded
/// duration: socket waits, process runs, outbound HTTP). Runs on a dedicated
/// waiter thread so it can never starve the bounded pool (plan 103 KTD2).
pub(crate) fn submit_host_wait<F>(task_id: i32, work: F)
where
    F: FnOnce() -> HostOpResult + Send + 'static,
{
    submit_host_class(task_id, super::boundary::JobClass::Wait, work)
}

fn submit_host_class<F>(task_id: i32, class: super::boundary::JobClass, work: F)
where
    F: FnOnce() -> HostOpResult + Send + 'static,
{
    super::boundary::with_boundary(|b| {
        b.submit(task_id, class, move || {
            Box::new(work()) as Box<dyn std::any::Any + Send>
        });
    });
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.host_op_begin(task_id, op_kind, arg_count, args_ptr) -> ()
    linker
        .func_wrap(
            "env",
            "host_op_begin",
            |mut caller: Caller<'_, ()>,
             task_id: i32,
             op_kind: i32,
             arg_count: i32,
             args_ptr: i32| {
                let args = match read_boxed_args(&mut caller, arg_count, args_ptr) {
                    Ok(args) => args,
                    Err(msg) => {
                        submit_host_error(task_id, msg);
                        return;
                    }
                };
                if super::net::begin_http_request_host_op(&mut caller, task_id, op_kind, &args) {
                    return;
                }
                if super::process::begin_process_host_op(&mut caller, task_id, op_kind, &args) {
                    return;
                }
                if super::io::begin_file_host_op(&mut caller, task_id, op_kind, &args) {
                    return;
                }
                if super::env::begin_env_host_op(&mut caller, task_id, op_kind, &args) {
                    return;
                }
                if super::sockets::begin_socket_host_op(&mut caller, task_id, op_kind, &args) {
                    return;
                }
                match op_kind {
                    fai_codegen_wasm::HOST_OP_ECHO_BOXED => {
                        let value = args.first().copied().unwrap_or(VAL_NULL);
                        submit_host_op(task_id, move || HostOpResult::RawI64(value));
                    }
                    _ => {
                        submit_host_error(task_id, format!("unknown host op kind {op_kind}"));
                    }
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.host_op_result(task_id) -> i64
    linker
        .func_wrap(
            "env",
            "host_op_result",
            |mut caller: Caller<'_, ()>, task_id: i32| -> i64 {
                let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                    return VAL_NULL;
                };
                match super::boundary::take_ready(task_id) {
                    Some(Ok(boxed)) => match boxed.downcast::<HostOpResult>() {
                        Ok(result) => marshal_result(&mut caller, &mem, *result),
                        Err(_) => signal_host_op_error(&mut caller, "host op result type mismatch"),
                    },
                    Some(Err(msg)) => signal_host_op_error(&mut caller, &msg),
                    None => signal_host_op_error(&mut caller, "missing host op result"),
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

fn read_boxed_args(
    caller: &mut Caller<'_, ()>,
    arg_count: i32,
    args_ptr: i32,
) -> Result<Vec<i64>, String> {
    if arg_count < 0 {
        return Err("negative host op arg count".to_string());
    }
    if args_ptr < 0 {
        return Err("negative host op args pointer".to_string());
    }
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        return Err("no guest memory".to_string());
    };
    let mut args = Vec::with_capacity(arg_count as usize);
    let data = mem.data(caller);
    for i in 0..arg_count as usize {
        let off = (args_ptr as usize)
            .checked_add(i * 8)
            .ok_or_else(|| "host op args pointer overflow".to_string())?;
        if off + 8 > data.len() {
            return Err("host op args out of bounds".to_string());
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&data[off..off + 8]);
        args.push(i64::from_le_bytes(buf));
    }
    Ok(args)
}

pub(crate) fn read_string_value(data: &[u8], val: i64) -> Option<&str> {
    let v = val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let addr = (v & ADDR_MASK) as usize;
    if addr + 8 > data.len() {
        return None;
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().ok()?);
    if tag != OBJ_TAG_STRING {
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

pub(crate) fn read_string_arg(
    caller: &mut Caller<'_, ()>,
    args: &[i64],
    idx: usize,
) -> Option<String> {
    let mem = caller.get_export("memory").and_then(|e| e.into_memory())?;
    let data = mem.data(&*caller);
    read_string_value(data, *args.get(idx)?).map(str::to_string)
}

pub(crate) fn read_int_value(val: i64) -> Option<i32> {
    let v = val as u64;
    if (v & (QNAN | SIGN_BIT | TAG_MASK)) != (QNAN | TAG_INT) {
        return None;
    }
    Some(v as u32 as i32)
}

fn submit_host_error(task_id: i32, msg: impl Into<String>) {
    let msg = msg.into();
    submit_host_op(task_id, move || HostOpResult::Error(msg));
}

fn marshal_result(caller: &mut Caller<'_, ()>, mem: &Memory, result: HostOpResult) -> i64 {
    match result {
        HostOpResult::RawI64(value) => value,
        HostOpResult::Json(value) => build_value(caller, mem, &value),
        HostOpResult::Null => VAL_NULL,
        HostOpResult::Int(value) => encode_int(value),
        HostOpResult::String(value) => wasm_alloc_str(caller, mem, &value),
        HostOpResult::TcpAccepted { stream, address } => {
            let conn_id = super::socket_registry::insert_tcp_stream(stream);
            let val = serde_json::json!({
                "handle": conn_id as i64,
                "address": address,
            });
            build_value(caller, mem, &val)
        }
        HostOpResult::TcpConnected(stream) => {
            let conn_id = super::socket_registry::insert_tcp_stream(stream);
            encode_int(conn_id as i32)
        }
        HostOpResult::EnvLoad { ok, pairs } => {
            if ok {
                for (key, value) in pairs {
                    // SAFETY: this preserves the existing `env.load` contract:
                    // mutate the process environment only after the async file
                    // read has completed and the task is resuming.
                    #[allow(unused_unsafe)]
                    unsafe {
                        std::env::set_var(key, value);
                    }
                }
            }
            build_value(caller, mem, &Value::Bool(ok))
        }
        HostOpResult::Error(msg) => signal_host_op_error(caller, &msg),
    }
}

fn encode_int(value: i32) -> i64 {
    (QNAN | TAG_INT | (value as u32 as u64)) as i64
}

fn signal_host_op_error(caller: &mut Caller<'_, ()>, message: &str) -> i64 {
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        return VAL_NULL;
    };
    let key_message = wasm_alloc_str(caller, &mem, "message");
    let key_kind = wasm_alloc_str(caller, &mem, "kind");
    let v_message = wasm_alloc_str(caller, &mem, message);
    let v_kind = wasm_alloc_str(caller, &mem, "host_op");
    let err_box = alloc_dict(
        caller,
        &mem,
        &[(key_message, v_message), (key_kind, v_kind)],
    );
    write_global_i32(caller, "__error_flag", 1);
    write_global_i64(caller, "__error_value", err_box);
    VAL_NULL
}
