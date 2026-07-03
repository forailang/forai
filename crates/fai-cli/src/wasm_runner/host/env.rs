//! Environment variable host imports: `env_get`, `env_load`. Mirrors
//! `std.env.get` and `std.env.load` from the forai stdlib.
//!
//! `env_get` reads from `std::env::var_os` and returns a NaN-boxed
//! String allocated on the guest heap, or `VAL_NULL` when the key is
//! unset. `env_load` parses a dotenv-style file (`KEY=VALUE` per line,
//! `#` comments, blank lines OK, single/double quotes stripped) and
//! merges its entries into the host process environment via
//! `std::env::set_var` so subsequent `env_get` calls see them.

use wasmtime::*;

use super::super::heap::wasm_alloc_str;
use super::super::nan_box::VAL_NULL;
use super::host_ops::{read_string_arg, submit_host_op, HostOpResult};

pub(super) fn begin_env_host_op(
    caller: &mut Caller<'_, ()>,
    task_id: i32,
    op_kind: i32,
    args: &[i64],
) -> bool {
    if op_kind != fai_codegen_wasm::HOST_OP_ENV_LOAD {
        return false;
    }
    let Some(path) = read_string_arg(caller, args, 0) else {
        submit_host_op(task_id, || HostOpResult::EnvLoad {
            ok: false,
            pairs: Vec::new(),
        });
        return true;
    };
    submit_host_op(task_id, move || match std::fs::read_to_string(&path) {
        Ok(content) => HostOpResult::EnvLoad {
            ok: true,
            pairs: parse_dotenv(&content),
        },
        Err(_) => HostOpResult::EnvLoad {
            ok: false,
            pairs: Vec::new(),
        },
    });
    true
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.env_get(key_ptr, key_len) -> i64 (NaN-boxed String | VAL_NULL)
    linker
        .func_wrap(
            "env",
            "env_get",
            |mut caller: Caller<'_, ()>, key_ptr: i32, key_len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let key = {
                    let data = mem.data(&caller);
                    read_slice(data, key_ptr, key_len)
                };
                match std::env::var(&key) {
                    Ok(value) => wasm_alloc_str(&mut caller, &mem, &value),
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.env_load(path_ptr, path_len) -> i32 (1=ok, 0=err)
    linker
        .func_wrap(
            "env",
            "env_load",
            |mut caller: Caller<'_, ()>, path_ptr: i32, path_len: i32| -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let path = {
                    let data = mem.data(&caller);
                    read_slice(data, path_ptr, path_len)
                };
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        for (key, value) in parse_dotenv(&content) {
                            // SAFETY: set_var is unsafe in newer Rust due to data races
                            // across threads reading env. The wasm runner is
                            // single-threaded for env-mutation purposes — guests call
                            // `env.load` from `main` before spawning workers.
                            #[allow(unused_unsafe)]
                            unsafe {
                                std::env::set_var(key, value);
                            }
                        }
                        1
                    }
                    Err(_) => 0,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

pub(super) fn read_slice(data: &[u8], ptr: i32, len: i32) -> String {
    let start = ptr as usize;
    let end = start.saturating_add(len as usize);
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

/// Parse a dotenv-style file body into `(key, value)` pairs.
///
/// Format:
/// - `KEY=VALUE` per line
/// - lines starting with `#` are comments
/// - blank lines are skipped
/// - leading/trailing whitespace around `KEY` and `VALUE` is trimmed
/// - if `VALUE` is wrapped in matching single or double quotes the
///   quotes are stripped (no escape handling)
/// - lines without `=` are skipped silently
pub(crate) fn parse_dotenv(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = strip_quotes(value);
        out.push((key.to_string(), value.to_string()));
    }
    out
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0];
        let last = bytes[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dotenv_basic() {
        let pairs = parse_dotenv("FOO=bar\nBAZ=qux\n");
        assert_eq!(
            pairs,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn parse_dotenv_skips_comments_and_blanks() {
        let pairs = parse_dotenv("# leading comment\n\nFOO=bar\n   # indented\n");
        assert_eq!(pairs, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn parse_dotenv_strips_matching_quotes() {
        let pairs = parse_dotenv("A=\"with spaces\"\nB='single'\nC=plain\n");
        assert_eq!(
            pairs,
            vec![
                ("A".to_string(), "with spaces".to_string()),
                ("B".to_string(), "single".to_string()),
                ("C".to_string(), "plain".to_string()),
            ]
        );
    }

    #[test]
    fn parse_dotenv_keeps_unmatched_quotes_intact() {
        let pairs = parse_dotenv("X=\"unterminated\nY='mismatch\"\n");
        assert_eq!(
            pairs,
            vec![
                ("X".to_string(), "\"unterminated".to_string()),
                ("Y".to_string(), "'mismatch\"".to_string()),
            ]
        );
    }

    #[test]
    fn parse_dotenv_trims_whitespace_around_key_and_value() {
        let pairs = parse_dotenv("  KEY  =  value  \n");
        assert_eq!(pairs, vec![("KEY".to_string(), "value".to_string())]);
    }

    #[test]
    fn parse_dotenv_skips_lines_without_equals() {
        let pairs = parse_dotenv("not an assignment\nFOO=bar\n");
        assert_eq!(pairs, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn parse_dotenv_skips_empty_keys() {
        let pairs = parse_dotenv("=value\nFOO=bar\n");
        assert_eq!(pairs, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn parse_dotenv_value_with_internal_equals_keeps_rest() {
        let pairs = parse_dotenv("URL=https://example.com/?a=1&b=2\n");
        assert_eq!(
            pairs,
            vec![(
                "URL".to_string(),
                "https://example.com/?a=1&b=2".to_string()
            )]
        );
    }
}
