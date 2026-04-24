//! Async/concurrency host imports: `now_ms`, `random`, `sleep_ms`, `run_all`, `spawn`.

use wasmtime::*;

use super::super::heap::decode_closure_header;
use super::super::nan_box::{encode_object, OBJ_TAG_TUPLE, VAL_NULL, VAL_VOID};

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.now_ms() -> f64
    linker
        .func_wrap("env", "now_ms", || -> f64 {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as f64
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // env.random() -> f64
    linker
        .func_wrap("env", "random", || -> f64 {
            use std::cell::Cell;
            thread_local! {
                static STATE: Cell<u64> = Cell::new(0x12345678_9abcdef0);
            }
            STATE.with(|s| {
                let mut x = s.get();
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                s.set(x);
                (x as f64) / (u64::MAX as f64)
            })
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // env.sleep_ms(ms: f64)
    linker
        .func_wrap("env", "sleep_ms", |ms: f64| {
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // env.run_all(args_ptr: i32, count: i32) -> i64
    // Reads N closure values from guest memory, calls each via the function table,
    // allocates a tuple [tag=2][count][val0][val1]... in guest memory, returns NaN-boxed pointer.
    linker
        .func_wrap(
            "env",
            "run_all",
            |mut caller: Caller<'_, ()>, args_ptr: i32, count: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();

                // Read closure values from guest memory
                let mut closure_vals = Vec::with_capacity(count as usize);
                {
                    let data = mem.data(&caller);
                    for i in 0..count {
                        let off = (args_ptr + i * 8) as usize;
                        if off + 8 <= data.len() {
                            let val = i64::from_le_bytes(data[off..off + 8].try_into().unwrap());
                            closure_vals.push(val as u64);
                        }
                    }
                }

                // Call each closure and collect results
                let mut results = Vec::with_capacity(closure_vals.len());
                for &closure_val in &closure_vals {
                    // Decode heap address from NaN-boxed object pointer
                    let addr = (closure_val & 0x0000_FFFF_FFFF_FFFF) as usize;

                    let header = {
                        let data = mem.data(&caller);
                        decode_closure_header(data, addr)
                    };
                    let header = match header {
                        Some(h) => h,
                        None => {
                            results.push(VAL_NULL);
                            continue;
                        }
                    };

                    // Set __env_ptr global so the closure can access upvalues
                    if let Some(env_global) = caller.get_export("__env_ptr") {
                        if let Some(g) = env_global.into_global() {
                            let _ = g.set(&mut caller, Val::I32(header.env_addr));
                        }
                    }

                    // Call the closure via the indirect function table
                    let result_val = call_via_table(&mut caller, header.table_idx);
                    results.push(result_val);
                }

                // Allocate tuple in guest memory: [tag:i32=2][count:i32][val0:i64][val1:i64]...
                let heap_ptr_val = match caller
                    .get_export("__heap_ptr")
                    .and_then(|e| e.into_global())
                {
                    Some(g) => match g.get(&mut caller) {
                        Val::I32(v) => v,
                        _ => return VAL_VOID,
                    },
                    None => return VAL_VOID,
                };

                let tuple_addr = heap_ptr_val as usize;
                let tuple_size = 8 + results.len() * 8; // tag(4) + count(4) + N * i64
                let new_heap_ptr = tuple_addr + tuple_size;

                // Write tuple to memory
                {
                    let data = mem.data_mut(&mut caller);
                    if tuple_addr + tuple_size > data.len() {
                        return VAL_VOID; // out of memory
                    }
                    data[tuple_addr..tuple_addr + 4].copy_from_slice(&OBJ_TAG_TUPLE.to_le_bytes());
                    data[tuple_addr + 4..tuple_addr + 8]
                        .copy_from_slice(&(results.len() as i32).to_le_bytes());
                    for (i, &val) in results.iter().enumerate() {
                        let off = tuple_addr + 8 + i * 8;
                        data[off..off + 8].copy_from_slice(&val.to_le_bytes());
                    }
                }

                // Bump heap_ptr
                if let Some(g) = caller
                    .get_export("__heap_ptr")
                    .and_then(|e| e.into_global())
                {
                    let _ = g.set(&mut caller, Val::I32(new_heap_ptr as i32));
                }

                // Return NaN-boxed object pointer to tuple
                encode_object(tuple_addr as u32)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.spawn(closure_val: i64) -> i64
    // Calls the closure synchronously (Tier 1 — no real concurrency in WASM yet).
    linker
        .func_wrap(
            "env",
            "spawn",
            |mut caller: Caller<'_, ()>, closure_val: i64| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let val = closure_val as u64;
                let addr = (val & 0x0000_FFFF_FFFF_FFFF) as usize;

                let header = {
                    let data = mem.data(&caller);
                    decode_closure_header(data, addr)
                };
                let header = match header {
                    Some(h) => h,
                    None => return VAL_VOID,
                };

                // Set __env_ptr
                if let Some(env_global) = caller.get_export("__env_ptr") {
                    if let Some(g) = env_global.into_global() {
                        let _ = g.set(&mut caller, Val::I32(header.env_addr));
                    }
                }

                // Call via table (result discarded)
                let _ = call_via_table(&mut caller, header.table_idx);
                VAL_VOID
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

/// Call `__indirect_function_table[table_idx]()` and return its i64 result.
/// Returns `VAL_NULL` if any step fails.
fn call_via_table(caller: &mut Caller<'_, ()>, table_idx: u32) -> i64 {
    let Some(table_export) = caller.get_export("__indirect_function_table") else {
        return VAL_NULL;
    };
    let Some(table) = table_export.into_table() else {
        return VAL_NULL;
    };
    let Some(func_ref) = table.get(&mut *caller, table_idx as u64) else {
        return VAL_NULL;
    };
    let Some(func) = func_ref.unwrap_func() else {
        return VAL_NULL;
    };
    let func = func.clone();
    let mut call_results = vec![Val::I64(0)];
    match func.call(&mut *caller, &[], &mut call_results) {
        Ok(()) => match call_results[0] {
            Val::I64(v) => v,
            _ => VAL_NULL,
        },
        Err(_) => VAL_NULL,
    }
}
