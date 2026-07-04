//! Post-mortem state dump (plan 116 phase 2).
//!
//! When a run dies (trap) or is killed by the watchdog, dump the state
//! that debugging sessions previously had to exfiltrate by hand: the
//! async task table (id, named resume fn, status, rstate, waiter, join
//! count, wake deadline) and heap stats (bump pointer, live objects,
//! free-list depths). Everything is read from exported globals plus a
//! walk of guest memory — no special debug build required.

use super::debug_table::DbgTable;
use wasmtime::{Instance, Store};

// Task record layout — mirrors `fai-codegen-wasm/src/async_engine.rs`.
const REC_SIZE: usize = 64;
const O_STATUS: usize = 0;
const O_RESUME: usize = 4;
const O_JOIN: usize = 12;
const O_WAKE: usize = 32;
const O_WAITER: usize = 44;
const O_RSTATE: usize = 48;

fn status_name(s: i32) -> &'static str {
    match s {
        0 => "READY",
        1 => "RUNNING",
        2 => "WAITING",
        3 => "COMPLETE",
        4 => "FAILED",
        5 => "FREED",
        _ => "?",
    }
}

fn global_i32(instance: &Instance, store: &mut Store<()>, name: &str) -> Option<i32> {
    instance
        .get_global(&mut *store, name)?
        .get(&mut *store)
        .i32()
}

fn read_i32(data: &[u8], addr: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        data.get(addr..addr + 4)?.try_into().ok()?,
    ))
}

fn read_f64(data: &[u8], addr: usize) -> Option<f64> {
    Some(f64::from_le_bytes(
        data.get(addr..addr + 8)?.try_into().ok()?,
    ))
}

/// Render the dump, or `None` when the module exposes nothing useful
/// (e.g. a hand-built fixture without the standard exports).
pub(crate) fn render(instance: &Instance, store: &mut Store<()>, dbg: &DbgTable) -> Option<String> {
    let heap_ptr = global_i32(instance, store, "__heap_ptr");
    let live_objects = global_i32(instance, store, "__live_objects");
    let free_list = global_i32(instance, store, "__free_list");
    // Scheduler globals — present only in async-engine builds.
    let count = global_i32(instance, store, "__dbg_count");
    let table_base = global_i32(instance, store, "__dbg_table_base");
    let current = global_i32(instance, store, "__dbg_current");
    let live_tasks = global_i32(instance, store, "__dbg_live");
    let root = global_i32(instance, store, "__dbg_root");

    let memory = instance.get_memory(&mut *store, "memory")?;
    let data = memory.data(&store);

    let mut out = String::from("post-mortem:");

    if let Some(hp) = heap_ptr {
        out.push_str(&format!("\n  heap: heap_ptr=0x{:x}", hp as u32));
        if let Some(live) = live_objects {
            out.push_str(&format!(", live objects={}", live));
        }
        let (blocks, bytes) = free_list_stats(data, dbg, free_list);
        out.push_str(&format!(", free blocks={} ({} bytes)", blocks, bytes));
    }

    if let (Some(count), Some(table_base)) = (count, table_base) {
        out.push_str(&format!(
            "\n  async tasks (count {}, live {}, current {}, root {}):",
            count.max(1) - 1,
            live_tasks.unwrap_or(-1),
            fmt_task_ref(current.unwrap_or(-1)),
            fmt_task_ref(root.unwrap_or(-1)),
        ));
        out.push_str("\n    id    status    rstate  waiter  joins  wake      resume");
        // Timer deadlines are absolute `now_ms()` values (unix epoch
        // ms on the native host) — render relative to now.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as f64;
        let mut freed = 0usize;
        for id in 1..count.max(1) {
            let rec = table_base as usize + id as usize * REC_SIZE;
            let Some(status) = read_i32(data, rec + O_STATUS) else {
                break;
            };
            if status == 5 {
                freed += 1;
                continue;
            }
            let resume = read_i32(data, rec + O_RESUME).unwrap_or(-1);
            let join = read_i32(data, rec + O_JOIN).unwrap_or(-1);
            let wake = read_f64(data, rec + O_WAKE).unwrap_or(-1.0);
            let waiter = read_i32(data, rec + O_WAITER).unwrap_or(-1);
            let rstate = read_i32(data, rec + O_RSTATE).unwrap_or(-1);
            let wake_str = if wake < 0.0 {
                "-".to_string()
            } else {
                // `in 59.8s` = parked on a timer; `0.2s ago` = due, the
                // next poll would promote it.
                let delta = wake - now_ms;
                if delta >= 0.0 {
                    format!("in {:.1}s", delta / 1000.0)
                } else {
                    format!("{:.1}s ago", -delta / 1000.0)
                }
            };
            out.push_str(&format!(
                "\n    t{:<4} {:<9} {:>6}  {:>6}  {:>5}  {:>8}  {}",
                id,
                status_name(status),
                rstate,
                fmt_task_ref(waiter),
                join,
                wake_str,
                dbg.table_slot_name(resume.max(0) as u32),
            ));
        }
        if freed > 0 {
            out.push_str(&format!("\n    ({} freed slot(s))", freed));
        }
    }

    Some(out)
}

/// Waiter / task-id field rendering: `-2` marks the host-driven root,
/// `-1` means none.
fn fmt_task_ref(id: i32) -> String {
    match id {
        -2 => "host".to_string(),
        i if i < 0 => "-".to_string(),
        i => format!("t{}", i),
    }
}

/// Count free blocks: the size-bucketed heads (region location from the
/// `fai-dbg` metadata) plus the linear overflow list. Each free node
/// stores `[size@0, next@4]`. Chain walks are capped defensively — the
/// dump runs after a trap, possibly over a corrupted heap.
fn free_list_stats(data: &[u8], dbg: &DbgTable, overflow_head: Option<i32>) -> (usize, u64) {
    const WALK_CAP: usize = 100_000;
    let mut blocks = 0usize;
    let mut bytes = 0u64;
    let mut walk = |mut node: i32| {
        let mut steps = 0;
        while node > 0 && steps < WALK_CAP {
            let Some(size) = read_i32(data, node as usize) else {
                break;
            };
            blocks += 1;
            bytes += size.max(0) as u64;
            node = read_i32(data, node as usize + 4).unwrap_or(0);
            steps += 1;
        }
    };
    if let Some((base, count)) = dbg.heap_buckets {
        for i in 0..count {
            if let Some(head) = read_i32(data, (base + i * 4) as usize) {
                walk(head);
            }
        }
    }
    if let Some(head) = overflow_head {
        walk(head);
    }
    (blocks, bytes)
}
