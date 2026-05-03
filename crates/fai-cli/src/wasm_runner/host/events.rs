//! Host-side event registry for `std.events`.
//!
//! `event_on(name, handler) -> Subscription` registers a closure under
//! a name; `emit(name, data)` synchronously invokes every subscriber
//! in registration order on a snapshot taken at the start of dispatch.
//! The registry uses a `thread_local!` HashMap, so a single-threaded
//! wasmtime instance shares state across emits without locking.
//!
//! Subscribers are ordinary fai closure handles (NaN-boxed); the host
//! invokes them by following the closure's `table_idx` into the guest
//! `__indirect_function_table`. This mirrors how
//! `host/http_server.rs::invoke_handler_with_err` invokes route
//! handlers — same pattern, different storage.

use std::cell::RefCell;
use std::collections::HashMap;
use wasmtime::*;

use super::super::heap::{decode_closure_header, wasm_alloc_str};
use super::super::nan_box::{
    encode_object, ADDR_MASK, OBJ_TAG_DICT, OBJ_TAG_STRING, QNAN, SIGN_BIT, TAG_INT, VAL_NULL,
};

#[derive(Clone)]
struct Subscription {
    id: i64,
    closure_val: i64,
    once: bool,
}

#[derive(Default)]
struct Registry {
    next_id: i64,
    by_name: HashMap<String, Vec<Subscription>>,
}

/// One queued deferred event. `data_val` is a NaN-boxed pointer into
/// guest memory (Dict / String / etc.), captured at `emitDeferred` time
/// and replayed verbatim on `drain`. The guest GC isn't moving so the
/// pointer stays valid between enqueue and drain — once we get a real
/// GC this needs to root the value.
#[derive(Clone)]
struct DeferredEvent {
    name: String,
    data_val: i64,
}

#[derive(Default)]
struct Queue {
    events: Vec<DeferredEvent>,
    /// Set while `drain` is running so a subscriber that throws (sets
    /// `__error_flag`) doesn't abort the whole drain — instead the
    /// drain loop catches the error, emits an `events:error` event with
    /// `{ name, message }` data, and continues. Recursive drain calls
    /// are no-ops (we're already draining; the new entries land in the
    /// queue and the active drain picks them up).
    draining: bool,
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
    static QUEUE: RefCell<Queue> = RefCell::new(Queue::default());
}

// ── Pure registry operations ────────────────────────────────────────
//
// Wasmtime-free helpers extracted so the registry's behaviour can be
// unit-tested without standing up a guest wasm instance. The
// `func_wrap` closures below call into these and add only the host
// glue (memory reads, subscription Dict construction, indirect call).

fn register(name: &str, closure_val: i64, once: bool) -> i64 {
    REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.next_id += 1;
        let id = reg.next_id;
        reg.by_name
            .entry(name.to_string())
            .or_default()
            .push(Subscription {
                id,
                closure_val,
                once,
            });
        id
    })
}

fn unregister(name: &str, id: i64) -> bool {
    REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        if let Some(list) = reg.by_name.get_mut(name) {
            let before = list.len();
            list.retain(|s| s.id != id);
            before != list.len()
        } else {
            false
        }
    })
}

/// Snapshot the live subscriber list for `name`, then drop every
/// `once` subscriber from the live registry. The returned snapshot
/// still includes the `once` entries — they fire one last time on
/// this dispatch — but a recursive emit during this dispatch sees
/// them already gone.
fn snapshot_and_drop_once(name: &str) -> Vec<Subscription> {
    REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        let snap = reg.by_name.get(name).cloned().unwrap_or_default();
        if let Some(list) = reg.by_name.get_mut(name) {
            list.retain(|s| !s.once);
        }
        snap
    })
}

fn count(name: &str) -> i32 {
    REGISTRY.with(|r| {
        r.borrow()
            .by_name
            .get(name)
            .map(|v| v.len() as i32)
            .unwrap_or(0)
    })
}

fn clear_name(name: &str) {
    REGISTRY.with(|r| {
        r.borrow_mut().by_name.remove(name);
    });
}

fn clear_all() {
    REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.by_name.clear();
        reg.next_id = 0;
    });
    QUEUE.with(|q| {
        let mut queue = q.borrow_mut();
        queue.events.clear();
        queue.draining = false;
    });
}

// ── Deferred queue ─────────────────────────────────────────────────

fn enqueue_deferred(name: &str, data_val: i64) {
    QUEUE.with(|q| {
        q.borrow_mut().events.push(DeferredEvent {
            name: name.to_string(),
            data_val,
        });
    });
}

fn queue_len() -> i32 {
    QUEUE.with(|q| q.borrow().events.len() as i32)
}

/// Pop the next queued event. Returns `None` when empty. Used by
/// `drain_queue` so subscribers can `emitDeferred` more events
/// during dispatch and the loop picks them up in the same pass.
fn pop_next_deferred() -> Option<DeferredEvent> {
    QUEUE.with(|q| {
        let mut queue = q.borrow_mut();
        if queue.events.is_empty() {
            None
        } else {
            Some(queue.events.remove(0))
        }
    })
}

fn set_draining(value: bool) {
    QUEUE.with(|q| q.borrow_mut().draining = value);
}

fn is_draining() -> bool {
    QUEUE.with(|q| q.borrow().draining)
}

/// Emit an event from the host side. `data_val` is a NaN-boxed value
/// the host has already constructed (Dict, Int, String, …). Used by
/// other host modules — `http_server.rs` for `http:beforeRequest` /
/// `http:afterResponse` / `http:listening` / `http:error` — that
/// need to fan out to fai subscribers without going through wasm.
///
/// Subscribers run synchronously in registration order on a snapshot
/// taken at entry. `once` subscribers are removed from the live
/// registry up front. If a subscriber throws (sets `error_flag`),
/// dispatch stops and the caller's post-call propagation delivers
/// the error.
pub(super) fn dispatch_event(caller: &mut Caller<'_, ()>, name: &str, data_val: i64) {
    let snapshot = snapshot_and_drop_once(name);
    if snapshot.is_empty() {
        return;
    }
    let event_val = build_event(caller, name, data_val);
    for sub in &snapshot {
        invoke_handler(caller, sub.closure_val, event_val);
        if get_error_flag(caller) != 0 {
            return;
        }
    }
}

/// Drain every queued deferred event in FIFO order. Subscribers can
/// `emitDeferred` more events during drain — those join the same pass
/// because we re-pop from the queue each iteration.
///
/// A subscriber that throws (sets `__error_flag`) does not abort the
/// drain. The error is read out, the flag is cleared, and an
/// `events:error` event with `{ name, message }` is dispatched (sync,
/// not deferred — observability subscribers want immediate visibility)
/// before the loop continues to the next queued entry.
///
/// Recursive `drain` calls are no-ops — the outer drain is already
/// pulling entries off the queue, and any subscriber pushing more
/// gets picked up by that loop.
pub(super) fn drain_queue(caller: &mut Caller<'_, ()>) {
    if is_draining() {
        return;
    }
    set_draining(true);
    while let Some(ev) = pop_next_deferred() {
        dispatch_event(caller, &ev.name, ev.data_val);
        if get_error_flag(caller) != 0 {
            // A subscriber threw during deferred dispatch. Clear the
            // flag, capture the error, and emit `events:error` so
            // observability handlers can recover without coupling to
            // the failing subscriber. Then continue draining.
            let message = take_error_message(caller);
            let err_val = build_events_error_payload(caller, &ev.name, &message);
            dispatch_event(caller, "events:error", err_val);
            // The events:error subscriber itself might have thrown —
            // in that case we do bail out (no second-order recovery).
            if get_error_flag(caller) != 0 {
                set_draining(false);
                return;
            }
        }
    }
    set_draining(false);
}

/// Read `__error_value`'s `message` field (an Error is a `{ message,
/// kind }` Dict at runtime), then clear both error globals so drain
/// can continue. Returns the message text; falls back to a generic
/// label if the error value can't be read for any reason.
fn take_error_message(caller: &mut Caller<'_, ()>) -> String {
    let err_val = read_global_i64(caller, "__error_value");
    let message = read_error_message(caller, err_val).unwrap_or_else(|| "(unknown)".to_string());
    write_global_i32(caller, "__error_flag", 0);
    write_global_i64(caller, "__error_value", 0);
    message
}

/// Pull the `message` field out of a NaN-boxed Error Dict. Returns
/// `None` for any malformed shape; the caller falls back to a
/// generic label.
fn read_error_message(caller: &mut Caller<'_, ()>, err_val: i64) -> Option<String> {
    let v = err_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let addr = (v & ADDR_MASK) as usize;
    let mem = caller.get_export("memory")?.into_memory()?;
    read_dict_string(&mem, caller, addr, "message")
}

/// Build the Dict payload for an `events:error` event:
/// `{ name: <event-name>, message: <error-message> }`.
fn build_events_error_payload(
    caller: &mut Caller<'_, ()>,
    failed_event: &str,
    message: &str,
) -> i64 {
    let mem = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .expect("memory export");
    let key_name = wasm_alloc_str(caller, &mem, "name");
    let key_message = wasm_alloc_str(caller, &mem, "message");
    let v_name = wasm_alloc_str(caller, &mem, failed_event);
    let v_message = wasm_alloc_str(caller, &mem, message);
    alloc_dict(
        caller,
        &mem,
        &[(key_name, v_name), (key_message, v_message)],
    )
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // event_on(name_ptr, name_len, closure_val) -> i64 (Subscription Dict)
    linker
        .func_wrap(
            "env",
            "event_on",
            |mut caller: Caller<'_, ()>,
             name_ptr: i32,
             name_len: i32,
             closure_val: i64|
             -> i64 {
                let name = read_name(&mut caller, name_ptr, name_len);
                let id = register(&name, closure_val, false);
                build_subscription(&mut caller, id, &name)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // event_once(name_ptr, name_len, closure_val) -> i64
    linker
        .func_wrap(
            "env",
            "event_once",
            |mut caller: Caller<'_, ()>,
             name_ptr: i32,
             name_len: i32,
             closure_val: i64|
             -> i64 {
                let name = read_name(&mut caller, name_ptr, name_len);
                let id = register(&name, closure_val, true);
                build_subscription(&mut caller, id, &name)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // event_off(sub_val) -> i32 (Bool)
    linker
        .func_wrap(
            "env",
            "event_off",
            |mut caller: Caller<'_, ()>, sub_val: i64| -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let Some((id, name)) = read_subscription(&mem, &mut caller, sub_val) else {
                    return 0;
                };
                if unregister(&name, id) {
                    1
                } else {
                    0
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // event_emit(name_ptr, name_len, data_val) -> void
    linker
        .func_wrap(
            "env",
            "event_emit",
            |mut caller: Caller<'_, ()>, name_ptr: i32, name_len: i32, data_val: i64| {
                let name = read_name(&mut caller, name_ptr, name_len);
                dispatch_event(&mut caller, &name, data_val);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // event_subscribers(name_ptr, name_len) -> i32
    linker
        .func_wrap(
            "env",
            "event_subscribers",
            |mut caller: Caller<'_, ()>, name_ptr: i32, name_len: i32| -> i32 {
                let name = read_name(&mut caller, name_ptr, name_len);
                count(&name)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // event_clear(name_ptr, name_len) -> void
    linker
        .func_wrap(
            "env",
            "event_clear",
            |mut caller: Caller<'_, ()>, name_ptr: i32, name_len: i32| {
                let name = read_name(&mut caller, name_ptr, name_len);
                clear_name(&name);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // event_clear_all() -> void
    linker
        .func_wrap("env", "event_clear_all", |_caller: Caller<'_, ()>| {
            clear_all();
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // event_emit_deferred(name_ptr, name_len, data_val) -> void
    linker
        .func_wrap(
            "env",
            "event_emit_deferred",
            |mut caller: Caller<'_, ()>, name_ptr: i32, name_len: i32, data_val: i64| {
                let name = read_name(&mut caller, name_ptr, name_len);
                enqueue_deferred(&name, data_val);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // event_drain() -> void
    linker
        .func_wrap("env", "event_drain", |mut caller: Caller<'_, ()>| {
            drain_queue(&mut caller);
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // event_queue_len() -> i32
    linker
        .func_wrap("env", "event_queue_len", |_caller: Caller<'_, ()>| -> i32 {
            queue_len()
        })
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

/// Read a `(ptr, len)` slice from guest memory as a `String`.
fn read_name(caller: &mut Caller<'_, ()>, ptr: i32, len: i32) -> String {
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let data = mem.data(&*caller);
    let start = ptr as usize;
    let end = start.saturating_add(len as usize);
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

/// Allocate a `Subscription { id, name }` Dict on the guest heap and
/// return its NaN-boxed pointer.
fn build_subscription(caller: &mut Caller<'_, ()>, id: i64, name: &str) -> i64 {
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let key_id = wasm_alloc_str(caller, &mem, "id");
    let key_name = wasm_alloc_str(caller, &mem, "name");
    let v_id = (QNAN | TAG_INT | (id as u32 as u64)) as i64;
    let v_name = wasm_alloc_str(caller, &mem, name);
    alloc_dict(caller, &mem, &[(key_id, v_id), (key_name, v_name)])
}

/// Allocate an `Event { name, data }` Dict on the guest heap.
fn build_event(caller: &mut Caller<'_, ()>, name: &str, data_val: i64) -> i64 {
    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
    let key_name = wasm_alloc_str(caller, &mem, "name");
    let key_data = wasm_alloc_str(caller, &mem, "data");
    let v_name = wasm_alloc_str(caller, &mem, name);
    alloc_dict(caller, &mem, &[(key_name, v_name), (key_data, data_val)])
}

/// Read `id` (Int) and `name` (String) out of a `Subscription` Dict.
fn read_subscription(
    mem: &Memory,
    caller: &mut Caller<'_, ()>,
    sub_val: i64,
) -> Option<(i64, String)> {
    let v = sub_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let addr = (v & ADDR_MASK) as usize;
    let id = read_dict_int(mem, caller, addr, "id")?;
    let name = read_dict_string(mem, caller, addr, "name")?;
    Some((id as i64, name))
}

/// Allocate a Dict on the guest heap from `(key_val, value_val)`
/// entries. Mirrors `host/http_server.rs::alloc_dict`. Bumps
/// `__heap_ptr` past a 16-slot capacity floor so subsequent
/// allocations don't overlap even when the caller keeps growing the
/// dict in place (we don't, but parity with the response builders is
/// worth maintaining).
pub(super) fn alloc_dict(caller: &mut Caller<'_, ()>, mem: &Memory, entries: &[(i64, i64)]) -> i64 {
    let addr = read_global_i32(caller, "__heap_ptr") as u32;
    let cap = std::cmp::max(entries.len(), 16);
    let data = mem.data_mut(&mut *caller);
    data[addr as usize..addr as usize + 4].copy_from_slice(&OBJ_TAG_DICT.to_le_bytes());
    data[addr as usize + 4..addr as usize + 8]
        .copy_from_slice(&(entries.len() as i32).to_le_bytes());
    for (i, (k, v)) in entries.iter().enumerate() {
        let ea = addr as usize + 8 + i * 16;
        data[ea..ea + 8].copy_from_slice(&k.to_le_bytes());
        data[ea + 8..ea + 16].copy_from_slice(&v.to_le_bytes());
    }
    let new_heap = align8(addr + 8 + cap as u32 * 16);
    write_global_i32(caller, "__heap_ptr", new_heap as i32);
    encode_object(addr)
}

fn align8(n: u32) -> u32 {
    (n + 7) & !7
}

fn read_global_i32(caller: &mut Caller<'_, ()>, name: &str) -> i32 {
    caller
        .get_export(name)
        .and_then(|e| e.into_global())
        .and_then(|g| match g.get(&mut *caller) {
            Val::I32(v) => Some(v),
            _ => None,
        })
        .unwrap_or(0)
}

pub(super) fn write_global_i32(caller: &mut Caller<'_, ()>, name: &str, val: i32) {
    if let Some(g) = caller.get_export(name).and_then(|e| e.into_global()) {
        let _ = g.set(&mut *caller, Val::I32(val));
    }
}

fn read_global_i64(caller: &mut Caller<'_, ()>, name: &str) -> i64 {
    caller
        .get_export(name)
        .and_then(|e| e.into_global())
        .and_then(|g| match g.get(&mut *caller) {
            Val::I64(v) => Some(v),
            _ => None,
        })
        .unwrap_or(0)
}

pub(super) fn write_global_i64(caller: &mut Caller<'_, ()>, name: &str, val: i64) {
    if let Some(g) = caller.get_export(name).and_then(|e| e.into_global()) {
        let _ = g.set(&mut *caller, Val::I64(val));
    }
}

fn get_error_flag(caller: &mut Caller<'_, ()>) -> i32 {
    read_global_i32(caller, "__error_flag")
}

/// Look up a key in a guest-heap Dict and return its String value.
fn read_dict_string(
    mem: &Memory,
    caller: &mut Caller<'_, ()>,
    addr: usize,
    key: &str,
) -> Option<String> {
    let entry = dict_lookup(mem, caller, addr, key)?;
    let v = entry as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let saddr = (v & ADDR_MASK) as usize;
    read_string_bytes(mem.data(&*caller), saddr).map(|s| s.to_string())
}

/// Look up a key in a guest-heap Dict and return its Int value.
fn read_dict_int(mem: &Memory, caller: &mut Caller<'_, ()>, addr: usize, key: &str) -> Option<i32> {
    let val = dict_lookup(mem, caller, addr, key)?;
    let v = val as u64;
    if (v & (QNAN | SIGN_BIT | 0x0007_0000_0000_0000)) == (QNAN | TAG_INT) {
        Some(v as i32)
    } else {
        None
    }
}

fn dict_lookup(mem: &Memory, caller: &mut Caller<'_, ()>, addr: usize, key: &str) -> Option<i64> {
    let data = mem.data(&*caller);
    if addr + 8 > data.len() {
        return None;
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().ok()?);
    if tag != OBJ_TAG_DICT {
        return None;
    }
    let count = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().ok()?) as usize;
    for i in 0..count {
        let ea = addr + 8 + i * 16;
        if ea + 16 > data.len() {
            break;
        }
        let k = i64::from_le_bytes(data[ea..ea + 8].try_into().ok()?);
        let v = i64::from_le_bytes(data[ea + 8..ea + 16].try_into().ok()?);
        let kv = k as u64;
        if (kv & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
            continue;
        }
        let kaddr = (kv & ADDR_MASK) as usize;
        if let Some(ks) = read_string_bytes(data, kaddr) {
            if ks == key {
                return Some(v);
            }
        }
    }
    None
}

fn read_string_bytes(data: &[u8], addr: usize) -> Option<&str> {
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
    std::str::from_utf8(&data[start..end]).ok()
}

/// Invoke a one-arg fai closure handler with the given NaN-boxed
/// argument. Mirrors `host/http_server.rs::invoke_handler` in shape.
/// Returns the closure's i64 result, or `VAL_NULL` if the handler
/// isn't a closure / table lookup fails — those cases are treated as
/// silent no-ops here because subscriber bodies are `Void`-returning
/// so the result is discarded anyway.
fn invoke_handler(caller: &mut Caller<'_, ()>, handler_val: i64, arg: i64) -> i64 {
    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return VAL_NULL,
    };
    let v = handler_val as u64;
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return VAL_NULL;
    }
    let addr = (v & ADDR_MASK) as usize;
    let header = {
        let data = mem.data(&*caller);
        match decode_closure_header(data, addr) {
            Some(h) => h,
            None => return VAL_NULL,
        }
    };
    if let Some(env_global) = caller.get_export("__env_ptr").and_then(|e| e.into_global()) {
        let _ = env_global.set(&mut *caller, Val::I32(header.env_addr));
    }
    let table = match caller
        .get_export("__indirect_function_table")
        .and_then(|e| e.into_table())
    {
        Some(t) => t,
        None => return VAL_NULL,
    };
    let func_ref = match table.get(&mut *caller, header.table_idx as u64) {
        Some(r) => r,
        None => return VAL_NULL,
    };
    let func = match func_ref.unwrap_func() {
        Some(f) => f.clone(),
        None => return VAL_NULL,
    };
    let mut results = vec![Val::I64(0)];
    if func
        .call(&mut *caller, &[Val::I64(arg)], &mut results)
        .is_err()
    {
        return VAL_NULL;
    }
    match results[0] {
        Val::I64(v) => v,
        _ => VAL_NULL,
    }
}

#[cfg(test)]
mod tests {
    //! Pure-Rust tests for the registry's bookkeeping. Closure
    //! invocation needs a wasmtime instance and is exercised by the
    //! `tests/fixtures/language/events/` end-to-end fixture.
    //!
    //! The registry is `thread_local!`, so each test must `clear_all`
    //! first to start from a known state — they share the test
    //! runner's thread.

    use super::*;

    #[test]
    fn register_appends_in_order_and_assigns_increasing_ids() {
        clear_all();
        let id1 = register("user:created", 100, false);
        let id2 = register("user:created", 101, false);
        let id3 = register("user:created", 102, false);
        assert!(id1 < id2 && id2 < id3);
        REGISTRY.with(|r| {
            let reg = r.borrow();
            let list = reg.by_name.get("user:created").unwrap();
            assert_eq!(list.len(), 3);
            assert_eq!(list[0].id, id1);
            assert_eq!(list[0].closure_val, 100);
            assert_eq!(list[1].closure_val, 101);
            assert_eq!(list[2].closure_val, 102);
        });
    }

    #[test]
    fn register_separates_by_name() {
        clear_all();
        register("a", 1, false);
        register("b", 2, false);
        register("a", 3, false);
        assert_eq!(count("a"), 2);
        assert_eq!(count("b"), 1);
        assert_eq!(count("missing"), 0);
    }

    #[test]
    fn unregister_removes_by_id_and_returns_true_only_when_active() {
        clear_all();
        let id1 = register("e", 10, false);
        let id2 = register("e", 20, false);
        assert!(unregister("e", id1));
        assert_eq!(count("e"), 1);
        // Second unregister of the same id is a no-op returning false.
        assert!(!unregister("e", id1));
        // Unregister the surviving one.
        assert!(unregister("e", id2));
        assert_eq!(count("e"), 0);
    }

    #[test]
    fn unregister_unknown_name_is_silent_no_op() {
        clear_all();
        assert!(!unregister("never-registered", 999));
    }

    #[test]
    fn unregister_finds_by_id_regardless_of_name_position() {
        clear_all();
        register("a", 1, false);
        let target = register("b", 2, false);
        register("c", 3, false);
        // off only matches the (name, id) pair stored at registration.
        // Calling unregister with the wrong name is a no-op even if the
        // id matches a different name's slot.
        assert!(!unregister("a", target));
        assert!(unregister("b", target));
    }

    #[test]
    fn snapshot_returns_subscribers_in_registration_order() {
        clear_all();
        register("e", 10, false);
        register("e", 20, false);
        register("e", 30, false);
        let snap = snapshot_and_drop_once("e");
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].closure_val, 10);
        assert_eq!(snap[1].closure_val, 20);
        assert_eq!(snap[2].closure_val, 30);
    }

    #[test]
    fn snapshot_is_independent_of_live_registry_mutations() {
        clear_all();
        register("e", 10, false);
        register("e", 20, false);
        let snap = snapshot_and_drop_once("e");
        // Adding a subscriber after the snapshot doesn't appear in it.
        register("e", 30, false);
        assert_eq!(snap.len(), 2);
        // But the live registry sees the new one.
        assert_eq!(count("e"), 3);
    }

    #[test]
    fn snapshot_drops_once_subscribers_from_live_registry() {
        clear_all();
        register("e", 10, false);
        register("e", 20, true); // once
        register("e", 30, false);
        register("e", 40, true); // once
        let snap = snapshot_and_drop_once("e");
        // Snapshot still has all 4 — they fire on this dispatch.
        assert_eq!(snap.len(), 4);
        // Live registry has only the non-once entries left.
        assert_eq!(count("e"), 2);
        let snap2 = snapshot_and_drop_once("e");
        assert_eq!(snap2.len(), 2);
        assert_eq!(snap2[0].closure_val, 10);
        assert_eq!(snap2[1].closure_val, 30);
    }

    #[test]
    fn snapshot_with_no_subscribers_returns_empty() {
        clear_all();
        let snap = snapshot_and_drop_once("never-registered");
        assert!(snap.is_empty());
    }

    #[test]
    fn count_returns_live_subscriber_count() {
        clear_all();
        assert_eq!(count("e"), 0);
        register("e", 1, false);
        register("e", 2, false);
        assert_eq!(count("e"), 2);
        unregister("e", 1);
        assert_eq!(count("e"), 1);
    }

    #[test]
    fn clear_name_drops_only_the_named_event() {
        clear_all();
        register("a", 1, false);
        register("b", 2, false);
        clear_name("a");
        assert_eq!(count("a"), 0);
        assert_eq!(count("b"), 1);
    }

    #[test]
    fn clear_all_drops_everything_and_resets_id_counter() {
        clear_all();
        register("a", 1, false);
        register("b", 2, false);
        clear_all();
        assert_eq!(count("a"), 0);
        assert_eq!(count("b"), 0);
        // After clear_all, ids restart from 1.
        let new_id = register("a", 10, false);
        assert_eq!(new_id, 1);
    }

    #[test]
    fn once_subscriber_alongside_persistent_keeps_persistent_after_dispatch() {
        clear_all();
        register("e", 10, false);
        register("e", 20, true);
        register("e", 30, false);
        snapshot_and_drop_once("e");
        // The two non-once subscribers survive a full dispatch cycle
        // and stay subscribed for the next emit.
        assert_eq!(count("e"), 2);
        let snap = snapshot_and_drop_once("e");
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].closure_val, 10);
        assert_eq!(snap[1].closure_val, 30);
    }
}
