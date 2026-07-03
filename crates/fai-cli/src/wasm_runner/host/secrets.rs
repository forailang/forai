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

use super::super::heap::{wasm_alloc_secret, wasm_alloc_str};
use super::super::nan_box::{ADDR_MASK, OBJ_TAG_SECRET, QNAN, SIGN_BIT};
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
    /// Values pre-resolved host-side by a non-env backend (dotenvx
    /// decryption, aws fetch) at startup validation. Never handed to the
    /// guest except through `secrets_reveal` and the egress points.
    /// Checked before the process environment in [`resolve_secret`].
    pub(crate) resolved: std::collections::HashMap<String, String>,
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
        crate::aws_secrets::clear();
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
/// Backend-resolved values win over the process environment: dotenvx
/// decrypts into the manifest's `resolved` map at startup; the aws
/// backend resolves through its host-side TTL cache
/// (stale-while-revalidate — no I/O on this thread). The env fallback
/// covers the `env` backend and lets a dotenvx/aws manifest still work
/// in dev with exported variables.
pub(crate) fn resolve_secret(name: &str) -> Option<String> {
    if let Some(v) = with_manifest(|m| m.and_then(|m| m.resolved.get(name).cloned())) {
        return Some(v);
    }
    if let Some(v) = crate::aws_secrets::resolve(name) {
        return Some(v);
    }
    std::env::var(name).ok()
}

/// The catchable message for a reveal that cannot resolve. Split out so
/// the value-free-diagnostics unit test can grep every rendered error
/// against seeded plaintext.
pub(crate) fn unresolvable_message(name: &str) -> String {
    format!(
        "secrets.reveal: secret '{}' could not be resolved (backend {})",
        name,
        active_backend()
    )
}

/// The catchable message for an undeclared `secrets.get` name.
pub(crate) fn undeclared_message(name: &str) -> String {
    format!(
        "secrets.get '{}' is not declared in [secrets] (backend {}). \
         Add `{} = {{}}` (or `{{ required = true }}`) to fai.toml",
        name,
        active_backend(),
        name
    )
}

/// Resolve a Secret handle payload to its egress value (plan 132 phase 3).
/// Payload grammar (built by the `secrets.bearer`/`basic`/`header`
/// combinators; a plain `secrets.get` handle is just the name):
///   "NAME"            → value
///   "bearer:NAME"     → "Bearer " + value
///   "basic:USER:NAME" → "Basic " + base64("USER:" + value)
/// `None` when the underlying name cannot be resolved by the backend.
pub(crate) fn resolve_secret_payload(payload: &str) -> Option<String> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    if let Some(name) = payload.strip_prefix("bearer:") {
        return resolve_secret(name).map(|v| format!("Bearer {}", v));
    }
    if let Some(rest) = payload.strip_prefix("basic:") {
        // The NAME is after the LAST colon — user names may contain ':'.
        let (user, name) = rest.rsplit_once(':')?;
        let value = resolve_secret(name)?;
        return Some(format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", user, value))
        ));
    }
    resolve_secret(payload)
}

/// If `text` is a rendered Secret redaction (`«secret PAYLOAD»`), return
/// the payload. The child-process env egress uses this: guest code embeds
/// a Secret in its env dict, stringify serializes only the redaction, and
/// the host swaps in the resolved value right before spawn. Resolution is
/// name-based and manifest-gated, so a hand-built redaction string grants
/// nothing an ordinary `secrets.get` would not.
pub(crate) fn redaction_payload(text: &str) -> Option<&str> {
    text.strip_prefix("«secret ")?.strip_suffix('»')
}

/// Decode a NaN-boxed value as a Secret handle and return its name.
/// `None` when the value isn't an object or isn't tagged SECRET.
pub(super) fn read_secret_name(data: &[u8], val: i64) -> Option<String> {
    let bits = val as u64;
    if (bits & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let addr = (bits & ADDR_MASK) as usize;
    if addr + 8 > data.len() {
        return None;
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().unwrap());
    if tag != OBJ_TAG_SECRET {
        return None;
    }
    let len = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().unwrap()).max(0) as usize;
    let end = addr.saturating_add(8).saturating_add(len);
    if end > data.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&data[addr + 8..end]).into_owned())
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
                    let msg = undeclared_message(&name);
                    return signal_host_error(&mut caller, "secrets", &msg);
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

    // env.secrets_refresh() -> i32 (count refetched). Blocking refetch
    // of every declared secret — an explicit rotation point for the aws
    // backend (TTL + stale-while-revalidate covers steady state). The
    // env/dotenvx backends have nothing to refetch and return 0.
    linker
        .func_wrap("env", "secrets_refresh", || -> i32 {
            crate::aws_secrets::refresh_all()
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // Egress combinators (plan 132 D1b): pure handle→handle transforms
    // that tag INTENT into the payload — the headers dict stays
    // Secret-valued and the host renders the final header bytes at
    // egress. No plaintext is touched here.
    // env.secrets_bearer(handle) -> i64 ("bearer:NAME" handle)
    linker
        .func_wrap(
            "env",
            "secrets_bearer",
            |mut caller: Caller<'_, ()>, handle: i64| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let name = {
                    let data = mem.data(&caller);
                    read_secret_name(data, handle)
                };
                let Some(name) = name else {
                    return signal_host_error(
                        &mut caller,
                        "secrets",
                        "secrets.bearer expects a Secret handle from secrets.get",
                    );
                };
                wasm_alloc_secret(&mut caller, &mem, &format!("bearer:{}", name))
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.secrets_basic(user_ptr, user_len, handle) -> i64
    // ("basic:USER:NAME" handle)
    linker
        .func_wrap(
            "env",
            "secrets_basic",
            |mut caller: Caller<'_, ()>, user_ptr: i32, user_len: i32, handle: i64| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let (user, name) = {
                    let data = mem.data(&caller);
                    (
                        read_slice(data, user_ptr, user_len),
                        read_secret_name(data, handle),
                    )
                };
                let Some(name) = name else {
                    return signal_host_error(
                        &mut caller,
                        "secrets",
                        "secrets.basic expects a Secret handle from secrets.get",
                    );
                };
                wasm_alloc_secret(&mut caller, &mem, &format!("basic:{}:{}", user, name))
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.secrets_header(handle) -> i64 (fresh copy of the same handle —
    // the explicit "use the raw value as this header" marker)
    linker
        .func_wrap(
            "env",
            "secrets_header",
            |mut caller: Caller<'_, ()>, handle: i64| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let name = {
                    let data = mem.data(&caller);
                    read_secret_name(data, handle)
                };
                let Some(name) = name else {
                    return signal_host_error(
                        &mut caller,
                        "secrets",
                        "secrets.header expects a Secret handle from secrets.get",
                    );
                };
                wasm_alloc_secret(&mut caller, &mem, &name)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.secrets_reveal(handle) -> i64 (fresh plaintext String). THE
    // audit anchor (plan 132 phase 2): the only import that moves secret
    // plaintext into guest memory. Failure paths raise catchable errors
    // that name the secret and backend — never a value.
    linker
        .func_wrap(
            "env",
            "secrets_reveal",
            |mut caller: Caller<'_, ()>, handle: i64| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let name = {
                    let data = mem.data(&caller);
                    read_secret_name(data, handle)
                };
                let Some(name) = name else {
                    return signal_host_error(
                        &mut caller,
                        "secrets",
                        "secrets.reveal expects a Secret handle from secrets.get",
                    );
                };
                match resolve_secret(&name) {
                    Some(value) => wasm_alloc_str(&mut caller, &mem, &value),
                    None => {
                        let msg = unresolvable_message(&name);
                        signal_host_error(&mut caller, "secrets", &msg)
                    }
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plan 132 phase 2 gate: every rendered secrets error names the
    /// secret and backend but provably never contains the resolved
    /// value. Seeds a real value into the process env, renders each
    /// message for that secret, and greps.
    #[test]
    fn error_messages_never_contain_secret_values() {
        let name = "FAI_TEST_SECRET_DIAGNOSTICS";
        let value = "plaintext-hunter2-do-not-print";
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(name, value);
        }
        assert_eq!(resolve_secret(name).as_deref(), Some(value));

        let rendered = [undeclared_message(name), unresolvable_message(name)];
        for msg in &rendered {
            assert!(
                !msg.contains(value),
                "diagnostic leaked a secret value: {}",
                msg
            );
            assert!(msg.contains(name), "diagnostic must name the secret: {}", msg);
        }
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn payload_grammar_resolves_schemes() {
        let name = "FAI_TEST_SECRET_PAYLOAD";
        let value = "tok-123";
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var(name, value);
        }
        assert_eq!(
            resolve_secret_payload(name).as_deref(),
            Some("tok-123"),
            "plain handle resolves to the raw value"
        );
        assert_eq!(
            resolve_secret_payload(&format!("bearer:{}", name)).as_deref(),
            Some("Bearer tok-123")
        );
        // basic: base64("user:tok-123")
        assert_eq!(
            resolve_secret_payload(&format!("basic:user:{}", name)).as_deref(),
            Some("Basic dXNlcjp0b2stMTIz")
        );
        // user names may contain ':' — the NAME is after the LAST colon.
        assert_eq!(
            resolve_secret_payload(&format!("basic:a:b:{}", name)).as_deref(),
            Some("Basic YTpiOnRvay0xMjM=")
        );
        assert_eq!(resolve_secret_payload("FAI_TEST_SECRET_PAYLOAD_UNSET"), None);
        assert_eq!(
            resolve_secret_payload("bearer:FAI_TEST_SECRET_PAYLOAD_UNSET"),
            None
        );
        #[allow(unused_unsafe)]
        unsafe {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn redaction_payload_roundtrip() {
        assert_eq!(redaction_payload("«secret API_KEY»"), Some("API_KEY"));
        assert_eq!(
            redaction_payload("«secret bearer:API_KEY»"),
            Some("bearer:API_KEY")
        );
        assert_eq!(redaction_payload("plain value"), None);
        assert_eq!(redaction_payload("«secret unterminated"), None);
    }

    #[test]
    fn read_secret_name_rejects_non_secret() {
        // A primitive (non-object) value is rejected outright.
        assert_eq!(read_secret_name(&[], 42), None);
        // An object-boxed addr pointing at a STRING-tagged block is
        // rejected — reveal must not read arbitrary strings.
        let mut data = vec![0u8; 32];
        data[0..4].copy_from_slice(&0i32.to_le_bytes()); // OBJ_TAG_STRING
        data[4..8].copy_from_slice(&3i32.to_le_bytes());
        data[8..11].copy_from_slice(b"abc");
        let boxed = (QNAN | SIGN_BIT) as i64;
        assert_eq!(read_secret_name(&data, boxed), None);
        // The same block tagged SECRET yields the name.
        data[0..4].copy_from_slice(&OBJ_TAG_SECRET.to_le_bytes());
        assert_eq!(read_secret_name(&data, boxed), Some("abc".to_string()));
    }
}
