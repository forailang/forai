//! Secret handle builtins — type signatures for `std.secrets` (plan 132).
//!
//! The wasm runtime implementation lives in
//! `fai-cli/src/wasm_runner/host/secrets.rs`. `secrets.get` returns an
//! opaque `Secret` handle carrying only the declared name; the host
//! resolves plaintext at egress, so the value never enters guest
//! memory. `secrets.has` probes whether the active backend can resolve
//! a name. `secrets.available` is the server/native availability probe
//! (browser builds receive stubs that return false; client code reaches
//! secrets only indirectly through `remote def` RPC).

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(b, "secretsGet", &[p("name", Type::String)], &[Type::Secret]);
    ins(b, "secretsHas", &[p("name", Type::String)], &[Type::Bool]);
    ins(b, "secretsAvailable", &[], &[Type::Bool]);
    // The single audit anchor: the only API that moves plaintext into
    // guest memory. `grep reveal` = the complete exposure audit.
    ins(b, "secretsReveal", &[p("secret", Type::Secret)], &[Type::String]);
    // Egress combinators (D1b): Secret → Secret intent tags. The host
    // renders the final header bytes at egress; plaintext stays host-side.
    ins(b, "secretsBearer", &[p("secret", Type::Secret)], &[Type::Secret]);
    ins(
        b,
        "secretsBasic",
        &[p("user", Type::String), p("secret", Type::Secret)],
        &[Type::Secret],
    );
    ins(b, "secretsHeader", &[p("secret", Type::Secret)], &[Type::Secret]);
    // Blocking refetch through the active backend's cache (aws rotation
    // point); env/dotenvx return 0.
    ins(b, "secretsRefresh", &[], &[Type::Int]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> HashMap<String, Type> {
        let mut b = HashMap::new();
        install(&mut b);
        b
    }

    #[test]
    fn test_secrets_builtins_registered() {
        let b = fresh();
        for name in &["secretsGet", "secretsHas", "secretsAvailable", "secretsReveal"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_secrets_get_returns_secret() {
        let b = fresh();
        match b.get("secretsGet").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 1);
                assert!(matches!(sig.params[0].ty, Type::String));
                assert!(matches!(sig.returns[0], Type::Secret));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_secrets_reveal_takes_secret_returns_string() {
        let b = fresh();
        match b.get("secretsReveal").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 1);
                assert!(matches!(sig.params[0].ty, Type::Secret));
                assert!(matches!(sig.returns[0], Type::String));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_secrets_probes_return_bool() {
        let b = fresh();
        for name in &["secretsHas", "secretsAvailable"] {
            match b.get(*name).unwrap() {
                Type::Function(sig) => assert!(matches!(sig.returns[0], Type::Bool)),
                _ => panic!("expected Function"),
            }
        }
    }
}
