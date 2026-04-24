//! Mock / spy host imports used by the `test` framework.
//!
//! Compile-time resolution turns `mock(fn, value)` and
//! `assert.calledWith(fn, ...)` into host calls keyed by a small
//! per-program `fn_id`. The host keeps a thread-local table of
//! mocks and call records; on the guest side, every top-level
//! function that appears as a mock/spy target in a `test` block
//! gets a preamble that consults `spy_check_call` before running
//! its real body.
//!
//! Spy assertion imports return `i32` — 0 on success, 1 on
//! failure (with a trap message already stashed via
//! `__fai_set_trap_msg`). The guest wraps the call in an `if`
//! that emits `unreachable` on 1, so the CLI test runner reads
//! the trap message via the existing `take_trap_msg` protocol.

use wasmtime::*;

use super::super::nan_box::{ADDR_MASK, OBJ_TAG_STRING, QNAN, SIGN_BIT};
use super::io::set_trap_msg;

/// Compare two NaN-boxed values for spy-assertion purposes.
/// Exact i64 equality first (handles Int, Bool, null, void, same
/// interned-string pointer). On miss, if both sides are String
/// objects, compare bytes. Everything else is "different" — the
/// assertion isn't structural beyond strings.
fn spy_args_equal(a: i64, b: i64, data: &[u8]) -> bool {
    if a == b {
        return true;
    }
    match (extract_string(a, data), extract_string(b, data)) {
        (Some(sa), Some(sb)) => sa == sb,
        _ => false,
    }
}

fn extract_string(val: i64, data: &[u8]) -> Option<&[u8]> {
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
    Some(&data[start..end])
}

#[derive(Debug, Clone, Default)]
struct SpyEntry {
    mock_value: Option<i64>,
    once_value: Option<i64>,
    calls: Vec<Vec<i64>>,
}

thread_local! {
    static SPY_STATE: std::cell::RefCell<Vec<SpyEntry>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

fn with_entry<R>(fn_id: i32, f: impl FnOnce(&mut SpyEntry) -> R) -> R {
    SPY_STATE.with(|s| {
        let mut v = s.borrow_mut();
        let idx = fn_id as usize;
        while v.len() <= idx {
            v.push(SpyEntry::default());
        }
        f(&mut v[idx])
    })
}

fn read_args(data: &[u8], args_ptr: i32, arg_count: i32) -> Vec<i64> {
    let start = args_ptr as usize;
    let mut out = Vec::with_capacity(arg_count.max(0) as usize);
    for i in 0..arg_count.max(0) as usize {
        let off = start + i * 8;
        if off + 8 > data.len() {
            break;
        }
        out.push(i64::from_le_bytes(data[off..off + 8].try_into().unwrap()));
    }
    out
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    linker
        .func_wrap("env", "spy_set_mock", |fn_id: i32, value: i64| {
            with_entry(fn_id, |e| {
                e.mock_value = Some(value);
                e.once_value = None;
            });
        })
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap("env", "spy_set_mock_once", |fn_id: i32, value: i64| {
            with_entry(fn_id, |e| {
                e.once_value = Some(value);
            });
        })
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap("env", "spy_reset", |fn_id: i32| {
            with_entry(fn_id, |e| {
                *e = SpyEntry::default();
            });
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // Every tracked function calls this first thing in its body:
    // records the args for `calledWith` / `callCount` assertions,
    // then reports whether a mock is set (writing the value to
    // `out_value_ptr` when mocked). Once-mocks consume on first hit.
    linker
        .func_wrap(
            "env",
            "spy_check_call",
            |mut caller: Caller<'_, ()>,
             fn_id: i32,
             args_ptr: i32,
             arg_count: i32,
             out_value_ptr: i32|
             -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let args = {
                    let data = mem.data(&caller);
                    read_args(data, args_ptr, arg_count)
                };
                let mocked = with_entry(fn_id, |e| {
                    e.calls.push(args);
                    if let Some(v) = e.once_value.take() {
                        Some(v)
                    } else {
                        e.mock_value
                    }
                });
                if let Some(v) = mocked {
                    let data = mem.data_mut(&mut caller);
                    let off = out_value_ptr as usize;
                    if off + 8 <= data.len() {
                        data[off..off + 8].copy_from_slice(&v.to_le_bytes());
                    }
                    1
                } else {
                    0
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "spy_assert_called_with",
            |mut caller: Caller<'_, ()>,
             fn_id: i32,
             expected_ptr: i32,
             expected_count: i32|
             -> i32 {
                let _ = &mut caller; // memory is read-only below
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let expected = read_args(data, expected_ptr, expected_count);
                let matched = SPY_STATE.with(|s| {
                    let v = s.borrow();
                    v.get(fn_id as usize)
                        .map(|e| {
                            e.calls.iter().any(|c| {
                                c.len() == expected.len()
                                    && c.iter()
                                        .zip(expected.iter())
                                        .all(|(a, b)| spy_args_equal(*a, *b, data))
                            })
                        })
                        .unwrap_or(false)
                });
                if matched {
                    0
                } else {
                    set_trap_msg(format!(
                        "spy assertion failed: fn_id {} not called with expected args",
                        fn_id,
                    ));
                    1
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "spy_assert_call_count",
            |fn_id: i32, expected: i32| -> i32 {
                let actual = SPY_STATE.with(|s| {
                    s.borrow()
                        .get(fn_id as usize)
                        .map(|e| e.calls.len() as i32)
                        .unwrap_or(0)
                });
                if actual == expected {
                    0
                } else {
                    set_trap_msg(format!(
                        "spy call count mismatch: fn_id {} expected {}, got {}",
                        fn_id, expected, actual,
                    ));
                    1
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap("env", "spy_assert_not_called", |fn_id: i32| -> i32 {
            let count = SPY_STATE.with(|s| {
                s.borrow()
                    .get(fn_id as usize)
                    .map(|e| e.calls.len())
                    .unwrap_or(0)
            });
            if count == 0 {
                0
            } else {
                set_trap_msg(format!(
                    "spy assertion failed: fn_id {} was called {} time(s)",
                    fn_id, count,
                ));
                1
            }
        })
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

/// Reset host-side spy state between test runs. The CLI test
/// runner calls this between cases so call history doesn't bleed.
pub(crate) fn reset_all() {
    SPY_STATE.with(|s| s.borrow_mut().clear());
}
