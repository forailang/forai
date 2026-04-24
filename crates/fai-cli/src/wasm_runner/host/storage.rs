//! Storage host imports: `storage_get`, `storage_set`, `storage_remove`,
//! `storage_clear`. Wasmtime-side backing is a thread-local HashMap —
//! persistence isn't the point here; we just need the imports to exist
//! so compiled programs that reference them can run inside the Wasmtime
//! test harness without trapping on missing imports.

use std::cell::RefCell;
use std::collections::HashMap;
use wasmtime::*;

thread_local! {
    static STORE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.storage_get(key_ptr, key_len, buf_ptr) -> value_len or -1
    linker
        .func_wrap(
            "env",
            "storage_get",
            |mut caller: Caller<'_, ()>, key_ptr: i32, key_len: i32, buf_ptr: i32| -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let key = {
                    let data = mem.data(&caller);
                    let end = (key_ptr + key_len) as usize;
                    if end > data.len() {
                        return -1;
                    }
                    std::str::from_utf8(&data[key_ptr as usize..end])
                        .unwrap_or("")
                        .to_string()
                };
                let value = STORE.with(|s| s.borrow().get(&key).cloned());
                match value {
                    Some(v) => {
                        let bytes = v.as_bytes();
                        let data = mem.data_mut(&mut caller);
                        let dst = buf_ptr as usize;
                        if dst + bytes.len() <= data.len() {
                            data[dst..dst + bytes.len()].copy_from_slice(bytes);
                            bytes.len() as i32
                        } else {
                            -1
                        }
                    }
                    None => -1,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.storage_set(key_ptr, key_len, val_ptr, val_len) -> void
    linker
        .func_wrap(
            "env",
            "storage_set",
            |mut caller: Caller<'_, ()>, key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let key_end = (key_ptr + key_len) as usize;
                let val_end = (val_ptr + val_len) as usize;
                if key_end > data.len() || val_end > data.len() {
                    return;
                }
                let key = std::str::from_utf8(&data[key_ptr as usize..key_end])
                    .unwrap_or("")
                    .to_string();
                let val = std::str::from_utf8(&data[val_ptr as usize..val_end])
                    .unwrap_or("")
                    .to_string();
                STORE.with(|s| s.borrow_mut().insert(key, val));
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.storage_remove(key_ptr, key_len) -> void
    linker
        .func_wrap(
            "env",
            "storage_remove",
            |mut caller: Caller<'_, ()>, key_ptr: i32, key_len: i32| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let end = (key_ptr + key_len) as usize;
                if end > data.len() {
                    return;
                }
                let key = std::str::from_utf8(&data[key_ptr as usize..end])
                    .unwrap_or("")
                    .to_string();
                STORE.with(|s| s.borrow_mut().remove(&key));
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.storage_clear() -> void
    linker
        .func_wrap("env", "storage_clear", |_caller: Caller<'_, ()>| {
            STORE.with(|s| s.borrow_mut().clear());
        })
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}
