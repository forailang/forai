//! Async/concurrency host imports.
//!
//! `host_set_timer` is the scheduler-owned sleep/all ABI: guest frames own
//! task state and ask the host only to arrange a later wakeup. Production
//! async (`fai run`, browser) lowers `sleep`/`all` through it.
//!
//! `sleep_ms` and `run_all` are the test-mode / legacy-direct compatibility
//! path. They are emitted only when async analysis declines (e.g. `is_test`
//! builds, where async functions called from `test` blocks fall through to the
//! direct builder); production async never reaches them. There they give
//! correct *values* for synchronous test assertions: `sleep_ms` blocks the
//! thread, `run_all` runs children sequentially. `spawn` backs `nowait` the
//! same way.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use wasmtime::*;

use super::super::heap::{decode_closure_header, host_retain, reserve};
use super::super::nan_box::{encode_object, OBJ_TAG_TUPLE, VAL_NULL, VAL_VOID};

thread_local! {
    /// task_id -> absolute deadline at which that task's guest `sleep` timer
    /// fires. The guest scheduler still decides expiry by polling time; the host
    /// keeps these deadlines so its driver loop can park until the *nearest* one
    /// instead of re-polling at a fixed fine cadence. The fixed cadence pegged a
    /// CPU core while a request handler was merely parked on a long outbound call
    /// (e.g. an LLM/MCP request taking tens of seconds), because each wake ran a
    /// full guest scheduler poll ~1000x/second for the whole duration.
    static TIMER_DEADLINES: RefCell<HashMap<i32, Instant>> = RefCell::new(HashMap::new());
}

#[cfg(test)]
pub(crate) fn clear_timer_requests() {
    TIMER_DEADLINES.with(|t| t.borrow_mut().clear());
}

/// Shortest time until any pending guest sleep timer fires, or `None` when none
/// are pending. A past-due timer reports `Duration::ZERO`, so the driver polls
/// promptly to let the guest scheduler resume it.
pub(crate) fn next_timer_timeout() -> Option<Duration> {
    let now = Instant::now();
    TIMER_DEADLINES.with(|t| {
        t.borrow()
            .values()
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
    })
}

/// How long a host driver loop may sleep before it must poll the guest scheduler
/// again: until the nearest pending timer, capped by a backstop so an untracked
/// wakeup source can't hang the loop, and floored so a just-due timer can't spin.
/// A boundary completion (outbound call, FFI offload) still wakes the loop
/// earlier through the condvar in `boundary::wait_for_ready`.
pub(crate) fn next_poll_timeout() -> Duration {
    // The backstop bounds how long the loop sleeps when nothing nearer is
    // tracked. It also bounds connection-accept latency on an idle server (the
    // loop accepts between polls), so it trades a little accept latency for a
    // large drop in idle/await CPU — at 250ms an idle server polls ~4x/sec
    // instead of 40x. A boundary completion still wakes the loop immediately.
    const BACKSTOP: Duration = Duration::from_millis(250);
    const FLOOR: Duration = Duration::from_millis(1);
    match next_timer_timeout() {
        Some(d) => d.clamp(FLOOR, BACKSTOP),
        None => BACKSTOP,
    }
}

/// Drop timers whose deadline has passed: the guest scheduler resumes their tasks
/// on the next poll, so the host need not wake for them again. Keeps the map
/// bounded over a long-running server.
pub(crate) fn prune_fired_timers() {
    let now = Instant::now();
    TIMER_DEADLINES.with(|t| t.borrow_mut().retain(|_, deadline| *deadline > now));
}

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
    // Test-mode / legacy-direct path. Production `sleep` lowers to
    // host_set_timer and never reaches this; here (async fns called from
    // `test` blocks) a real blocking sleep makes the synchronous test
    // assertion observe the post-suspend value.
    linker
        .func_wrap("env", "sleep_ms", |ms: f64| {
            std::thread::sleep(std::time::Duration::from_millis(ms.max(0.0) as u64));
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // env.host_set_timer(task_id: i32, ms: i32)
    // Records when this task's sleep timer fires as an absolute deadline, so the
    // host driver loop can sleep until the nearest pending timer instead of
    // re-polling at a fixed fine cadence. The guest scheduler still resumes the
    // task by polling time; the host only needs to be awake at the deadline.
    linker
        .func_wrap("env", "host_set_timer", |task_id: i32, ms: i32| {
            let deadline = Instant::now() + Duration::from_millis(ms.max(0) as u64);
            TIMER_DEADLINES.with(|t| {
                t.borrow_mut().insert(task_id, deadline);
            });
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // env.run_all(args_ptr: i32, count: i32) -> i64
    // Test-mode / legacy-direct path. Production `all` lowers to guest-owned
    // child task records via the scheduler; this is reached only when async
    // analysis declines (e.g. `is_test`). Reads N closure values from guest
    // memory, calls each via the function table (sequentially — tests assert
    // values, not overlap), allocates a tuple
    // [tag=2][count][val0][val1]... in guest memory, returns NaN-boxed pointer.
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
                // Route through `reserve` so the tuple carries the rc=1 prefix
                // the guest RC expects (plan 113). The tuple co-owns each result
                // it collects (a closure may have returned a borrowed value), so
                // each object element is retained — releasing whatever else holds
                // it then can't free it out from under this tuple.
                let tuple_size = 8 + results.len() * 8; // tag(4) + count(4) + N * i64
                let tuple_addr = reserve(&mut caller, &mem, tuple_size) as usize;

                {
                    let data = mem.data_mut(&mut caller);
                    data[tuple_addr..tuple_addr + 4].copy_from_slice(&OBJ_TAG_TUPLE.to_le_bytes());
                    data[tuple_addr + 4..tuple_addr + 8]
                        .copy_from_slice(&(results.len() as i32).to_le_bytes());
                    for (i, &val) in results.iter().enumerate() {
                        let off = tuple_addr + 8 + i * 8;
                        data[off..off + 8].copy_from_slice(&val.to_le_bytes());
                    }
                    for &val in &results {
                        host_retain(data, val);
                    }
                }

                // Return NaN-boxed object pointer to tuple
                encode_object(tuple_addr as u32)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.spawn(closure_val: i64) -> i64
    // Legacy nowait compatibility path. Calls the closure synchronously until
    // nowait is moved onto guest-owned task records.
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

#[cfg(test)]
mod tests {
    use super::*;

    // Seed a deadline `ms_from_now` milliseconds away (negative = already past).
    fn set_deadline(task_id: i32, ms_from_now: i64) {
        let now = Instant::now();
        let deadline = if ms_from_now >= 0 {
            now + Duration::from_millis(ms_from_now as u64)
        } else {
            now - Duration::from_millis((-ms_from_now) as u64)
        };
        TIMER_DEADLINES.with(|t| {
            t.borrow_mut().insert(task_id, deadline);
        });
    }

    #[test]
    fn next_timeout_is_none_when_no_timers() {
        clear_timer_requests();
        assert_eq!(next_timer_timeout(), None);
    }

    #[test]
    fn next_timeout_picks_the_nearest_deadline() {
        clear_timer_requests();
        set_deadline(1, 5000);
        set_deadline(2, 200);
        set_deadline(3, 9000);
        let t = next_timer_timeout().expect("a timer is pending");
        // Nearest is task 2 (~200ms); allow a little scheduling slack.
        assert!(
            t <= Duration::from_millis(200),
            "expected <=200ms, got {t:?}"
        );
        assert!(
            t >= Duration::from_millis(100),
            "expected >=100ms, got {t:?}"
        );
        clear_timer_requests();
    }

    #[test]
    fn past_due_timer_reports_zero_and_poll_timeout_floors_to_1ms() {
        clear_timer_requests();
        set_deadline(7, -10);
        assert_eq!(next_timer_timeout(), Some(Duration::ZERO));
        // A due timer must poll promptly but never spin at a zero-length wait.
        assert_eq!(next_poll_timeout(), Duration::from_millis(1));
        clear_timer_requests();
    }

    #[test]
    fn poll_timeout_is_backstopped_when_idle_or_far() {
        clear_timer_requests();
        // No timers pending -> the backstop, so the loop never sleeps forever and
        // a boundary completion still wakes it earlier via the condvar.
        assert_eq!(next_poll_timeout(), Duration::from_millis(250));
        // A far-off timer is capped to the backstop, not waited on in full.
        set_deadline(1, 10_000);
        assert_eq!(next_poll_timeout(), Duration::from_millis(250));
        clear_timer_requests();
    }

    #[test]
    fn prune_drops_only_fired_timers() {
        clear_timer_requests();
        set_deadline(1, -5);
        set_deadline(2, 5000);
        prune_fired_timers();
        TIMER_DEADLINES.with(|t| {
            let map = t.borrow();
            assert!(!map.contains_key(&1), "a fired timer should be pruned");
            assert!(map.contains_key(&2), "a pending timer should remain");
        });
        clear_timer_requests();
    }
}
