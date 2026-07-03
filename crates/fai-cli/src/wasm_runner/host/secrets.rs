//! Secret handle host imports (plan 132): `secrets_get`, `secrets_has`,
//! `secrets_available`. Mirrors `std.secrets` from the forai stdlib.
//!
//! `secrets_get` allocates an opaque `OBJ_TAG_SECRET` handle on the guest
//! heap carrying only the secret NAME — plaintext never enters guest
//! memory; host ops that accept a handle resolve it host-side at egress.
//! `secrets_has` probes whether the active backend can resolve a name
//! (never returning the value). `secrets_available` is the native-target
//! availability probe (the browser twin returns 0).
//!
//! The env backend reads the host process environment, which includes
//! entries merged by `env.load` / dotenv startup loading.

use wasmtime::*;

use super::super::heap::wasm_alloc_secret;
use super::env::read_slice;

/// Resolve a secret name to plaintext on the HOST side. This is the single
/// resolution point the egress paths (and later backends) go through; it
/// must never be exposed to the guest as a return value outside
/// `secrets_reveal` (phase 2) and declared egress positions (phase 3).
pub(crate) fn resolve_secret(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.secrets_get(name_ptr, name_len) -> i64 (NaN-boxed Secret handle)
    linker
        .func_wrap(
            "env",
            "secrets_get",
            |mut caller: Caller<'_, ()>, name_ptr: i32, name_len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let name = {
                    let data = mem.data(&caller);
                    read_slice(data, name_ptr, name_len)
                };
                wasm_alloc_secret(&mut caller, &mem, &name)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.secrets_has(name_ptr, name_len) -> i32 (1 = resolvable)
    linker
        .func_wrap(
            "env",
            "secrets_has",
            |mut caller: Caller<'_, ()>, name_ptr: i32, name_len: i32| -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let name = {
                    let data = mem.data(&caller);
                    read_slice(data, name_ptr, name_len)
                };
                resolve_secret(&name).is_some() as i32
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.secrets_available() -> i32 (always 1 on the native host)
    linker
        .func_wrap("env", "secrets_available", || -> i32 { 1 })
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}
