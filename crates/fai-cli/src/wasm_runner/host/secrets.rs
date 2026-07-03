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
//! The active `[secrets]` manifest is installed by the run entry point
//! via `SecretsGuard` (the `ExternGuard` pattern from `util.rs`). With a
//! manifest, `secrets_get` on an undeclared name raises a catchable
//! guest error naming the secret and backend — never a value. Without
//! one (loose single-file runs), any name is allowed against the `env`
//! backend, which reads the host process environment (including entries
//! merged by `env.load`).

use std::cell::RefCell;

use wasmtime::*;

use super::super::heap::wasm_alloc_secret;
use super::env::read_slice;
use super::host_ops::signal_host_error;

/// The active `[secrets]` manifest for the current run, already filtered
/// to the active target's declarations by the CLI.
#[derive(Debug, Clone, Default)]
pub(crate) struct SecretsManifest {
    /// Backend name: "env" (default) | "dotenvx" | "aws".
    pub(crate) backend: String,
    /// Declared secret names visible to this run's target.
    pub(crate) declared: Vec<String>,
}

thread_local! {
    /// Manifest for the currently-running wasm module. Populated by the
    /// run entry point before calling into the module, cleared on the
    /// way out (same single-thread-to-completion argument as
    /// `util::CURRENT_EXTERNS`). `None` = no `[secrets]` section.
    static CURRENT_MANIFEST: RefCell<Option<SecretsManifest>> = const { RefCell::new(None) };
}

/// Install the secrets manifest for an upcoming `run_wasm*`. Returned
/// guard clears the thread-local on drop so the next run starts clean.
pub(crate) struct SecretsGuard;

impl SecretsGuard {
    pub(crate) fn set(manifest: Option<SecretsManifest>) -> Self {
        CURRENT_MANIFEST.with(|slot| *slot.borrow_mut() = manifest);
        SecretsGuard
    }
}

impl Drop for SecretsGuard {
    fn drop(&mut self) {
        CURRENT_MANIFEST.with(|slot| *slot.borrow_mut() = None);
    }
}

fn with_manifest<R>(f: impl FnOnce(Option<&SecretsManifest>) -> R) -> R {
    CURRENT_MANIFEST.with(|slot| f(slot.borrow().as_ref()))
}

/// The active backend name, for diagnostics ("env" when no manifest).
pub(crate) fn active_backend() -> String {
    with_manifest(|m| {
        m.map(|m| m.backend.clone())
            .unwrap_or_else(|| "env".to_string())
    })
}

/// Whether `name` is declared for this run. `None` manifest = no
/// declaration requirement (loose single-file mode).
fn undeclared(name: &str) -> bool {
    with_manifest(|m| m.is_some_and(|m| !m.declared.iter().any(|d| d == name)))
}

/// Resolve a secret name to plaintext on the HOST side. This is the single
/// resolution point the egress paths (and later backends) go through; it
/// must never be exposed to the guest as a return value outside
/// `secrets_reveal` (phase 2) and declared egress positions (phase 3).
///
/// Backends beyond `env` land in later phases (dotenvx: phase 4, aws:
/// phase 5); until then every backend name resolves via process env so a
/// dotenvx/aws manifest still works in dev with exported variables.
pub(crate) fn resolve_secret(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.secrets_get(name_ptr, name_len) -> i64 (NaN-boxed Secret handle).
    // Undeclared name (when a manifest exists) raises a catchable guest
    // error via the __error_flag channel — the message names the secret
    // and backend, never a value.
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
                if undeclared(&name) {
                    return signal_host_error(
                        &mut caller,
                        "secrets",
                        &format!(
                            "secrets.get '{}' is not declared in [secrets] (backend {}). \
                             Add `{} = {{}}` (or `{{ required = true }}`) to fai.toml",
                            name,
                            active_backend(),
                            name
                        ),
                    );
                }
                wasm_alloc_secret(&mut caller, &mem, &name)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.secrets_has(name_ptr, name_len) -> i32 (1 = resolvable).
    // Undeclared names probe as 0 rather than erroring — `has` is the
    // guard callers use to branch, so it must be safe to call.
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
                if undeclared(&name) {
                    return 0;
                }
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
