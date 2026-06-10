//! Array higher-order host imports: `array_map`, `array_filter`,
//! `array_find`, `array_is_any`, `array_is_all`. Each reads the guest-side
//! Array, invokes a guest closure once per element via
//! `__indirect_function_table`, and builds the aggregated result back on
//! the guest heap.
//!
//! Mirrors `native_map`/`native_filter`/`native_find`/`native_is_any`/
//! `native_is_all` in fai-runtime — the VM implementations there return
//! `VAL_NULL` today (stubs), but Phase C wires real semantics on the wasm
//! side so forai code can actually depend on them.

use wasmtime::*;

use super::super::heap::{host_retain, reserve, wasm_alloc_str};
use super::super::nan_box::{
    encode_object, ADDR_MASK, OBJ_TAG_ARRAY, OBJ_TAG_CLOSURE, QNAN, SIGN_BIT, TAG_BOOL, TAG_NULL,
    TAG_VOID, VAL_NULL,
};

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    linker
        .func_wrap(
            "env",
            "array_map",
            |mut caller: Caller<'_, ()>, arr_val: i64, closure_val: i64| -> i64 {
                let items = match read_array(&mut caller, arr_val) {
                    Some(v) => v,
                    None => return VAL_NULL,
                };
                let mut results: Vec<i64> = Vec::with_capacity(items.len());
                for elem in items {
                    match invoke_closure(&mut caller, closure_val, elem) {
                        Some(r) => results.push(r),
                        None => return VAL_NULL,
                    }
                }
                alloc_array_of(&mut caller, &results)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "array_filter",
            |mut caller: Caller<'_, ()>, arr_val: i64, closure_val: i64| -> i64 {
                let items = match read_array(&mut caller, arr_val) {
                    Some(v) => v,
                    None => return VAL_NULL,
                };
                let mut kept: Vec<i64> = Vec::new();
                for elem in items {
                    match invoke_closure(&mut caller, closure_val, elem) {
                        Some(r) if is_truthy(r) => kept.push(elem),
                        Some(_) => {}
                        None => return VAL_NULL,
                    }
                }
                alloc_array_of(&mut caller, &kept)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "array_find",
            |mut caller: Caller<'_, ()>, arr_val: i64, closure_val: i64| -> i64 {
                let items = match read_array(&mut caller, arr_val) {
                    Some(v) => v,
                    None => return VAL_NULL,
                };
                for elem in items {
                    match invoke_closure(&mut caller, closure_val, elem) {
                        Some(r) if is_truthy(r) => return elem,
                        Some(_) => {}
                        None => return VAL_NULL,
                    }
                }
                VAL_NULL
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "array_is_any",
            |mut caller: Caller<'_, ()>, arr_val: i64, closure_val: i64| -> i64 {
                let items = match read_array(&mut caller, arr_val) {
                    Some(v) => v,
                    None => return encode_bool(false),
                };
                for elem in items {
                    match invoke_closure(&mut caller, closure_val, elem) {
                        Some(r) if is_truthy(r) => return encode_bool(true),
                        Some(_) => {}
                        None => return encode_bool(false),
                    }
                }
                encode_bool(false)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "array_is_all",
            |mut caller: Caller<'_, ()>, arr_val: i64, closure_val: i64| -> i64 {
                let items = match read_array(&mut caller, arr_val) {
                    Some(v) => v,
                    None => return encode_bool(false),
                };
                for elem in items {
                    match invoke_closure(&mut caller, closure_val, elem) {
                        Some(r) if !is_truthy(r) => return encode_bool(false),
                        Some(_) => {}
                        None => return encode_bool(false),
                    }
                }
                encode_bool(true)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // Touch `wasm_alloc_str` to silence unused-import warnings — it's used
    // only in sibling host modules but the mod-level `use` is convenient.
    let _ = wasm_alloc_str as fn(_, _, _) -> _;

    Ok(())
}

/// Read an Array from guest memory. Returns `None` for anything that isn't
/// a NaN-boxed Array object pointer or whose memory is out of range.
fn read_array(caller: &mut Caller<'_, ()>, val: i64) -> Option<Vec<i64>> {
    let mem = caller.get_export("memory")?.into_memory()?;
    let data = mem.data(&*caller);
    let v = val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let addr = (v & ADDR_MASK) as usize;
    if addr + 8 > data.len() {
        return None;
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().ok()?);
    if tag != OBJ_TAG_ARRAY {
        return None;
    }
    let count = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().ok()?) as usize;
    let mut out: Vec<i64> = Vec::with_capacity(count);
    for i in 0..count {
        let off = addr + 8 + i * 8;
        if off + 8 > data.len() {
            return None;
        }
        out.push(i64::from_le_bytes(data[off..off + 8].try_into().ok()?));
    }
    Some(out)
}

/// Invoke a guest closure with one i64 argument, returning the i64 result
/// or `None` if the closure is invalid or the call fails. Mirrors the
/// `env.spawn` invocation pattern used in fai-codegen-wasm tests.
fn invoke_closure(caller: &mut Caller<'_, ()>, closure_val: i64, arg: i64) -> Option<i64> {
    let mem = caller.get_export("memory")?.into_memory()?;
    let v = closure_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let addr = (v & ADDR_MASK) as usize;

    let (table_idx, env_addr) = {
        let data = mem.data(&*caller);
        if addr + 16 > data.len() {
            return None;
        }
        let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().ok()?);
        if tag != OBJ_TAG_CLOSURE {
            return None;
        }
        let tidx = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().ok()?);
        (tidx as u32, (addr + 16) as i32)
    };

    // Point `__env_ptr` at this closure's upvalue payload so any
    // GetUpvalue reads during the call land on the right bytes.
    if let Some(eg) = caller.get_export("__env_ptr") {
        if let Some(g) = eg.into_global() {
            let _ = g.set(&mut *caller, Val::I32(env_addr));
        }
    }

    let table = caller
        .get_export("__indirect_function_table")?
        .into_table()?;
    let func_ref = table.get(&mut *caller, table_idx as u64)?;
    let func = func_ref.unwrap_func()?.clone();
    let mut results = vec![Val::I64(0)];
    func.call(&mut *caller, &[Val::I64(arg)], &mut results)
        .ok()?;
    match results[0] {
        Val::I64(x) => Some(x),
        _ => None,
    }
}

/// Allocate an Array on the guest heap with the given NaN-boxed items.
///
/// Routes through `reserve` so the array carries the 8-byte rc=1 prefix the
/// guest RC expects (without it, binding/releasing the result reads a garbage
/// count 8 bytes early). The result array co-owns each element it stores —
/// `filter` keeps references shared with the source array, and a projecting
/// `map` closure can return a borrowed element — so each object element is
/// retained; releasing the source later then can't free them out from under
/// this array. Over-retaining a freshly-built `map` result only leaks, never a
/// UAF.
fn alloc_array_of(caller: &mut Caller<'_, ()>, items: &[i64]) -> i64 {
    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return VAL_NULL,
    };
    let need = 8 + items.len() * 8;
    let addr = reserve(caller, &mem, need) as usize;
    let data = mem.data_mut(&mut *caller);
    data[addr..addr + 4].copy_from_slice(&OBJ_TAG_ARRAY.to_le_bytes());
    data[addr + 4..addr + 8].copy_from_slice(&(items.len() as i32).to_le_bytes());
    for (i, v) in items.iter().enumerate() {
        let off = addr + 8 + i * 8;
        data[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    for v in items {
        host_retain(data, *v);
    }
    encode_object(addr as u32)
}

/// VM-parity truthiness: `false`, `null`, and `void` are falsy; everything
/// else (including 0 and empty strings) is truthy. Matches `val_to_bool`
/// in fai-runtime.
fn is_truthy(val: i64) -> bool {
    let v = val as u64;
    if v == (QNAN | TAG_VOID) || v == (QNAN | TAG_NULL) {
        return false;
    }
    if v == (QNAN | TAG_BOOL) {
        return false;
    }
    true
}

fn encode_bool(b: bool) -> i64 {
    let bits = QNAN | TAG_BOOL | if b { 1 } else { 0 };
    bits as i64
}
