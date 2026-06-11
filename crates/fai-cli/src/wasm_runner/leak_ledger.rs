//! Heap allocation ledger (plan 116 phase 5, `--check-leaks`).
//!
//! The inverse of the scalar `__live_objects` counter: an itemized
//! record of every live heap block, so a leak names itself. Debug-only
//! `__fai_alloc_event(addr, size)` / `__fai_free_event(addr, size)`
//! imports (emitted in `rt_alloc` / `rt_free` when the build was made
//! with `--check-leaks`) insert/remove entries; host-side allocations
//! (`heap::reserve`, which bypasses `rt_alloc` but whose objects are
//! freed through `rt_free`) are recorded too, so their frees match.
//!
//! Tier 1: the live set grouped by size — "56 live: 56B".
//! Tier 2a: each allocation captures a wasm backtrace; the report
//! resolves the first non-runtime frames through the `fai-dbg` table
//! → `Dict 56B allocated in buildContainer ← renderSSR ← handle`.
//!
//! Frees with no matching live allocation surface double-free /
//! free-of-unknown-address (heap corruption) — complementing
//! `FAI_RC_CHECK`, which only sees rc-prefixed objects.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::debug_table::DbgTable;
use wasmtime::{Instance, Store};

/// Max wasm frames kept per allocation record.
const MAX_FRAMES: usize = 10;
/// Max anomalous-free samples kept for the report.
const MAX_ANOMALY_SAMPLES: usize = 8;
/// Max groups listed in the final report.
const MAX_REPORT_GROUPS: usize = 40;

/// One live allocation: logical size, origin, and the captured wasm
/// backtrace (function indices, youngest first) for site attribution.
#[derive(Clone)]
struct AllocRecord {
    size: u32,
    /// True when allocated host-side via `heap::reserve` (bypasses
    /// `rt_alloc`, so it never bumps `__live_objects`).
    host: bool,
    frames: Vec<u32>,
}

#[derive(Default)]
struct Ledger {
    enabled: bool,
    map: HashMap<u32, AllocRecord>,
    /// Total host-side (`reserve`) allocations ever — the self-check
    /// offset: `__live_objects` counts guest allocs minus all frees,
    /// the ledger counts guest+host allocs minus matched frees, so
    /// `map.len() - host_allocs - unknown_frees == __live_objects`.
    host_allocs: u64,
    /// Guest instrumentation events seen (alloc + free). Zero after a
    /// run means the module was NOT built with `--check-leaks`.
    guest_events: u64,
    unknown_frees: u64,
    unknown_free_samples: Vec<(u32, u32)>,
    /// Last allocation record for recently-freed addresses, so a
    /// free-list corruption trap can name the victim block even though
    /// it left the live map at free time. Cleared wholesale when it
    /// grows past a bound — debug-mode memory hygiene, not accounting.
    recent_frees: HashMap<u32, AllocRecord>,
    live_bytes: u64,
    /// Periodic summary for servers (`--check-leaks=interval:N`).
    interval: Option<Duration>,
    last_report: Option<Instant>,
    prev_live: usize,
    prev_bytes: u64,
    /// Debug table for resolving allocation sites in interval reports
    /// (a server blocked in the host accept loop never reaches the
    /// exit report, so the interval line is its only channel).
    dbg: Option<Rc<DbgTable>>,
}

thread_local! {
    static LEDGER: RefCell<Ledger> = RefCell::new(Ledger::default());
}

/// Arm (or disarm) the ledger for the run starting on this thread,
/// clearing any state from a previous run.
pub(crate) fn reset(enabled: bool, interval_ms: Option<u64>, dbg: Option<Rc<DbgTable>>) {
    LEDGER.with(|l| {
        *l.borrow_mut() = Ledger {
            enabled,
            interval: interval_ms.map(Duration::from_millis),
            dbg,
            ..Ledger::default()
        };
    });
}

/// Cheap guard for call sites that pay a capture cost (backtraces).
pub(crate) fn is_enabled() -> bool {
    LEDGER.with(|l| l.borrow().enabled)
}

/// Capture the current wasm backtrace as function indices (youngest
/// first, capped). `ctx` is any store context — a host `Caller` works.
pub(crate) fn capture_frames(ctx: impl wasmtime::AsContext) -> Vec<u32> {
    wasmtime::WasmBacktrace::capture(ctx)
        .frames()
        .iter()
        .take(MAX_FRAMES)
        .map(|f| f.func_index())
        .collect()
}

/// Record an allocation. `host` marks host-side `reserve` allocations.
/// Returns `true` when an interval summary is due — the caller should
/// then render one via [`interval_report`] (split so the report can
/// read guest memory, which the ledger itself can't reach).
pub(crate) fn record_alloc(addr: u32, size: u32, host: bool, frames: Vec<u32>) -> bool {
    LEDGER.with(|l| {
        let mut led = l.borrow_mut();
        if !led.enabled {
            return false;
        }
        if host {
            led.host_allocs += 1;
        } else {
            led.guest_events += 1;
        }
        led.live_bytes += size as u64;
        // An alloc at an address that's already live means we missed a
        // free (shouldn't happen — rt_free covers every path). Replace;
        // the stale entry would otherwise double-count forever.
        if let Some(old) = led.map.insert(addr, AllocRecord { size, host, frames }) {
            led.live_bytes = led.live_bytes.saturating_sub(old.size as u64);
        }
        // Interval clock: the first alloc starts it; due after each
        // full interval since the last report.
        let interval = match led.interval {
            Some(i) => i,
            None => return false,
        };
        let now = Instant::now();
        match led.last_report {
            Some(prev) if now.duration_since(prev) < interval => false,
            None => {
                led.last_report = Some(now);
                led.prev_live = led.map.len();
                led.prev_bytes = led.live_bytes;
                false
            }
            _ => {
                led.last_report = Some(now);
                true
            }
        }
    })
}

/// Record a free. Unmatched addresses are kept as anomalies (double
/// free or free of a never-allocated address).
pub(crate) fn record_free(addr: u32, size: u32) {
    LEDGER.with(|l| {
        let mut led = l.borrow_mut();
        if !led.enabled {
            return;
        }
        led.guest_events += 1;
        match led.map.remove(&addr) {
            Some(rec) => {
                led.live_bytes = led.live_bytes.saturating_sub(rec.size as u64);
                if led.recent_frees.len() > 100_000 {
                    led.recent_frees.clear();
                }
                led.recent_frees.insert(addr, rec);
            }
            None => {
                led.unknown_frees += 1;
                if led.unknown_free_samples.len() < MAX_ANOMALY_SAMPLES {
                    led.unknown_free_samples.push((addr, size));
                }
            }
        }
    });
}

/// Name the block at `addr` (a logical object pointer) for trap
/// reports: its live/freed state, size, and allocation site chain.
/// Lets a free-list corruption trap say WHAT got scribbled, which
/// usually identifies the writer. `None` when the ledger is disarmed
/// or the address was never recorded.
pub(crate) fn describe_block(addr: u32) -> Option<String> {
    LEDGER.with(|l| {
        let led = l.borrow();
        if !led.enabled {
            return None;
        }
        let (rec, state) = match led.map.get(&addr) {
            Some(r) => (r.clone(), "live"),
            None => match led.recent_frees.get(&addr) {
                Some(r) => (r.clone(), "freed"),
                None => return None,
            },
        };
        let dbg_owned;
        let dbg: &DbgTable = match &led.dbg {
            Some(d) => d,
            None => {
                dbg_owned = DbgTable::default();
                &dbg_owned
            }
        };
        let mut sites: Vec<String> = Vec::new();
        for idx in &rec.frames {
            let Some(name) = dbg.func_name(*idx) else {
                continue;
            };
            if name.starts_with("rt_") || name.starts_with("__") {
                continue;
            }
            sites.push(name.to_string());
            if sites.len() == 3 {
                break;
            }
        }
        let site = if sites.is_empty() {
            if rec.host {
                "<host import>".to_string()
            } else {
                "<runtime>".to_string()
            }
        } else if rec.host {
            format!("host import ← {}", sites.join(" ← "))
        } else {
            sites.join(" ← ")
        };
        Some(format!(
            "{} {}B block allocated in {}",
            state, rec.size, site
        ))
    })
}

/// Render the periodic `--check-leaks=interval:N` report: a compact
/// growth line plus the top live groups WITH allocation sites — for a
/// server (which never reaches the exit report, and may sit blocked in
/// the host accept loop where the watchdog can't reach), this line is
/// the leak report. `data` is the guest memory (for object tags); call
/// only when [`record_alloc`] returned `true`.
pub(crate) fn interval_report(data: &[u8]) -> Option<String> {
    LEDGER.with(|l| {
        let mut led = l.borrow_mut();
        if !led.enabled {
            return None;
        }
        let live = led.map.len();
        let bytes = led.live_bytes;
        let mut out = format!(
            "[check-leaks] live={} ({:+}) bytes={} ({:+})",
            live,
            live as i64 - led.prev_live as i64,
            bytes,
            bytes as i64 - led.prev_bytes as i64,
        );
        led.prev_live = live;
        led.prev_bytes = bytes;
        let dbg_owned;
        let dbg: &DbgTable = match &led.dbg {
            Some(d) => d,
            None => {
                dbg_owned = DbgTable::default();
                &dbg_owned
            }
        };
        out.push_str(&render_groups(&led.map, data, dbg, 6));
        Some(out)
    })
}

/// Render the final live-set report: Tier 1 (count × size), Tier 2a
/// (allocation site resolved through the `fai-dbg` table), the
/// `__live_objects` self-check, and any free anomalies. `None` when
/// the ledger is disarmed.
pub(crate) fn render_report(
    instance: &Instance,
    store: &mut Store<()>,
    dbg: &DbgTable,
) -> Option<String> {
    let (map, host_allocs, guest_events, unknown_frees, samples, live_bytes) = LEDGER.with(|l| {
        let led = l.borrow();
        if !led.enabled {
            return None;
        }
        Some((
            led.map.clone(),
            led.host_allocs,
            led.guest_events,
            led.unknown_frees,
            led.unknown_free_samples.clone(),
            led.live_bytes,
        ))
    })?;

    if guest_events == 0 {
        return Some(
            "[check-leaks] no allocation events — this module was not built with \
             --check-leaks (rebuild from source with the flag, or set FAI_CHECK_LEAKS=1)"
                .to_string(),
        );
    }

    let live_objects = instance
        .get_global(&mut *store, "__live_objects")
        .and_then(|g| g.get(&mut *store).i32());
    let memory = instance.get_memory(&mut *store, "memory");
    let data: &[u8] = match &memory {
        Some(m) => m.data(&*store),
        None => &[],
    };

    let mut out = format!(
        "[check-leaks] live heap: {} objects, {} bytes",
        map.len(),
        live_bytes,
    );
    // Self-check: the ledger and the runtime's scalar counter must
    // describe the same heap. `__live_objects` only counts guest
    // (`rt_alloc`) allocations but is decremented by every `rt_free`,
    // host-allocated objects included — so the expected counter value
    // is the ledger minus host allocations minus unmatched frees.
    if let Some(counter) = live_objects {
        let expected = map.len() as i64 - host_allocs as i64 - unknown_frees as i64;
        if counter as i64 == expected {
            out.push_str(&format!(
                " (__live_objects={} consistent; {} host-side)",
                counter, host_allocs,
            ));
        } else {
            out.push_str(&format!(
                " (SELF-CHECK MISMATCH: __live_objects={} but ledger implies {}; \
                 {} host-side allocs, {} unknown frees)",
                counter, expected, host_allocs, unknown_frees,
            ));
        }
    }

    out.push_str(&render_groups(&map, data, dbg, MAX_REPORT_GROUPS));

    if unknown_frees > 0 {
        out.push_str(&format!(
            "\n  {} free(s) with no live allocation (double-free or heap corruption); first:",
            unknown_frees,
        ));
        for (addr, size) in &samples {
            out.push_str(&format!("\n    free(0x{:x}, {}B)", addr, size));
        }
    }
    Some(out)
}

/// Render the live set grouped by (size, site-chain) — the by-size
/// Tier-1 view with Tier-2a attribution attached. `group_label` folds
/// in the tag read from the live object header, so identical shapes
/// group together. Lines are prefixed with `\n`; empty map → empty
/// string.
fn render_groups(
    map: &HashMap<u32, AllocRecord>,
    data: &[u8],
    dbg: &DbgTable,
    max_groups: usize,
) -> String {
    let mut groups: HashMap<(u32, String), (usize, u64)> = HashMap::new();
    for (addr, rec) in map {
        let label = group_label(*addr, rec, data, dbg);
        let entry = groups.entry((rec.size, label)).or_default();
        entry.0 += 1;
        entry.1 += rec.size as u64;
    }
    let mut rows: Vec<((u32, String), (usize, u64))> = groups.into_iter().collect();
    rows.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then(b.1 .0.cmp(&a.1 .0)));

    let mut out = String::new();
    if rows.is_empty() {
        return out;
    }
    out.push_str("\n  count    bytes  what");
    for ((size, label), (count, bytes)) in rows.iter().take(max_groups) {
        out.push_str(&format!(
            "\n  {:>5} {:>8}  {:>6}B  {}",
            count, bytes, size, label,
        ));
    }
    if rows.len() > max_groups {
        out.push_str(&format!("\n  (+{} more groups)", rows.len() - max_groups));
    }
    out
}

/// `Tag allocated in a ← b ← c` for one record: object tag read from
/// the live header, then the first few non-runtime backtrace frames
/// resolved through the debug table.
fn group_label(addr: u32, rec: &AllocRecord, data: &[u8], dbg: &DbgTable) -> String {
    let tag = read_i32(data, addr as usize).map(tag_name).unwrap_or("?");
    let mut sites: Vec<String> = Vec::new();
    for idx in &rec.frames {
        let Some(name) = dbg.func_name(*idx) else {
            continue;
        };
        // Skip allocator/runtime internals — the user wants the forai
        // function that asked for memory, not rt_alloc itself.
        if name.starts_with("rt_") || name.starts_with("__") {
            continue;
        }
        sites.push(name.to_string());
        if sites.len() == 3 {
            break;
        }
    }
    let site = if sites.is_empty() {
        if rec.host {
            "<host import>".to_string()
        } else {
            "<runtime>".to_string()
        }
    } else if rec.host {
        format!("host import ← {}", sites.join(" ← "))
    } else {
        sites.join(" ← ")
    };
    format!("{} allocated in {}", tag, site)
}

fn tag_name(tag: i32) -> &'static str {
    use super::nan_box::*;
    match tag {
        t if t == OBJ_TAG_STRING => "String",
        t if t == OBJ_TAG_ARRAY => "Array",
        t if t == OBJ_TAG_TUPLE => "Tuple",
        t if t == OBJ_TAG_DICT => "Dict",
        t if t == OBJ_TAG_CLOSURE => "Closure",
        8 => "Cell",
        _ => "Object",
    }
}

fn read_i32(data: &[u8], addr: usize) -> Option<i32> {
    Some(i32::from_le_bytes(data.get(addr..addr + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_tracks_live_set_and_anomalies() {
        reset(true, None, None);
        assert!(is_enabled());
        record_alloc(0x100, 56, false, vec![]);
        record_alloc(0x200, 56, false, vec![]);
        record_alloc(0x300, 24, true, vec![]);
        record_free(0x100, 56);
        record_free(0x100, 56); // double free → anomaly
        LEDGER.with(|l| {
            let led = l.borrow();
            assert_eq!(led.map.len(), 2);
            assert_eq!(led.live_bytes, 56 + 24);
            assert_eq!(led.host_allocs, 1);
            assert_eq!(led.unknown_frees, 1);
            // Self-check arithmetic: __live_objects would be
            // guest allocs (2) - frees (2) = 0 == 2 - 1 - 1.
            assert_eq!(led.map.len() as i64 - led.host_allocs as i64 - led.unknown_frees as i64, 0);
        });
        reset(false, None, None);
        assert!(!is_enabled());
    }

    #[test]
    fn disabled_ledger_records_nothing() {
        reset(false, None, None);
        record_alloc(0x100, 56, false, vec![]);
        record_free(0x200, 8);
        LEDGER.with(|l| {
            let led = l.borrow();
            assert!(led.map.is_empty());
            assert_eq!(led.unknown_frees, 0);
        });
    }
}
