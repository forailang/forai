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
//! failure. On failure they raise through the error channel
//! (`signal_host_error` sets `__error_flag`/`__error_value` with the
//! message); the guest drops the i32 and runs the post-call propagation,
//! so a failed spy assertion unwinds `try`/`catch`/`finally` like
//! `assert.*` instead of hard-trapping past cleanup.

use wasmtime::*;

use super::super::heap::{host_release_value, host_release_values, host_retain_value};
use super::super::nan_box::{ADDR_MASK, OBJ_TAG_STRING, QNAN, SIGN_BIT};
use super::host_ops::signal_host_error;

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
    // Call records are borrowed snapshots for same-test assertions. They do not
    // own guest retain credits; only mock_value/once_value are host-retained.
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
        .func_wrap(
            "env",
            "spy_set_mock",
            |mut caller: Caller<'_, ()>, fn_id: i32, value: i64| {
                host_retain_value(&mut caller, value);
                let old = with_entry(fn_id, |e| {
                    let old = [e.mock_value.take(), e.once_value.take()];
                    e.mock_value = Some(value);
                    e.once_value = None;
                    old
                });
                host_release_values(&mut caller, old.into_iter().flatten());
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "spy_set_mock_once",
            |mut caller: Caller<'_, ()>, fn_id: i32, value: i64| {
                host_retain_value(&mut caller, value);
                let old = with_entry(fn_id, |e| {
                    let old = e.once_value.take();
                    e.once_value = Some(value);
                    old
                });
                host_release_values(&mut caller, old);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "spy_reset",
            |mut caller: Caller<'_, ()>, fn_id: i32| {
                let old = with_entry(fn_id, |e| {
                    let old = retained_values(e);
                    *e = SpyEntry::default();
                    old
                });
                host_release_values(&mut caller, old);
            },
        )
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
                        Some((v, true))
                    } else {
                        e.mock_value.map(|v| (v, false))
                    }
                });
                if let Some((v, release_stored_credit)) = mocked {
                    // The spy registry owns the stored mock credit. Each mocked
                    // return gives the guest a fresh result credit before a
                    // once-mock drops the registry's credit.
                    host_retain_value(&mut caller, v);
                    if release_stored_credit {
                        host_release_value(&mut caller, v);
                    }
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
                    // Raise through the error channel (a catchable throw that
                    // runs `finally`) rather than a hard trap, matching
                    // `assert.*`. The guest runs the post-call propagation.
                    let _ = signal_host_error(
                        &mut caller,
                        "assertion",
                        &format!(
                            "spy assertion failed: fn_id {} not called with expected args",
                            fn_id,
                        ),
                    );
                    1
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "spy_assert_call_count",
            |mut caller: Caller<'_, ()>, fn_id: i32, expected: i32| -> i32 {
                let actual = SPY_STATE.with(|s| {
                    s.borrow()
                        .get(fn_id as usize)
                        .map(|e| e.calls.len() as i32)
                        .unwrap_or(0)
                });
                if actual == expected {
                    0
                } else {
                    let _ = signal_host_error(
                        &mut caller,
                        "assertion",
                        &format!(
                            "spy call count mismatch: fn_id {} expected {}, got {}",
                            fn_id, expected, actual,
                        ),
                    );
                    1
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "spy_assert_not_called",
            |mut caller: Caller<'_, ()>, fn_id: i32| -> i32 {
                let count = SPY_STATE.with(|s| {
                    s.borrow()
                        .get(fn_id as usize)
                        .map(|e| e.calls.len())
                        .unwrap_or(0)
                });
                if count == 0 {
                    0
                } else {
                    let _ = signal_host_error(
                        &mut caller,
                        "assertion",
                        &format!(
                            "spy assertion failed: fn_id {} was called {} time(s)",
                            fn_id, count,
                        ),
                    );
                    1
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

fn retained_values(entry: &mut SpyEntry) -> Vec<i64> {
    [entry.mock_value.take(), entry.once_value.take()]
        .into_iter()
        .flatten()
        .collect()
}

/// Drain host-retained spy/mock values between test runs. The CLI test runner
/// releases the returned handles through the active instance's `__fai_release`
/// export, then the cleared state prevents call history from bleeding.
pub(crate) fn drain_retained_values() -> Vec<i64> {
    SPY_STATE.with(|s| {
        let mut state = s.borrow_mut();
        let mut retained = Vec::new();
        for entry in state.iter_mut() {
            retained.extend(retained_values(entry));
        }
        state.clear();
        retained
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_state() {
        SPY_STATE.with(|s| s.borrow_mut().clear());
    }

    #[test]
    fn drain_retained_values_returns_mock_slots_and_clears_state() {
        clear_state();
        with_entry(2, |e| {
            e.mock_value = Some(11);
            e.once_value = Some(22);
            e.calls.push(vec![33]);
        });

        let retained = drain_retained_values();
        assert_eq!(retained, vec![11, 22]);

        let retained_again = drain_retained_values();
        assert!(retained_again.is_empty());
    }
}
