//! Misc host imports: `call_ffi` (routes through the `fai-ffi` bridge
//! to `libloading`-backed C calls) and `float_to_str`.

use std::cell::RefCell;

use wasmtime::*;

use super::super::nan_box::{format_float, ADDR_MASK, OBJ_TAG_STRING, QNAN, SIGN_BIT, VAL_NULL};
use fai_ffi::{FfiType, PtrTracker};

/// Per-extern declaration the host needs to dispatch a call.
#[derive(Debug, Clone)]
pub struct ExternInfo {
    pub library: String,
    pub function: String,
    pub param_types: Vec<FfiType>,
    pub return_type: FfiType,
}

thread_local! {
    /// Extern declarations for the currently-running wasm module, keyed
    /// by the `ext_fn_idx` the guest supplies to `call_ffi`. Populated
    /// by the run entry point before calling into the module, cleared
    /// on the way out. A thread-local is enough because `run_wasm`
    /// runs the module to completion on this thread — the same trick
    /// `output::CaptureGuard` uses for stdout capture.
    static CURRENT_EXTERNS: RefCell<Vec<ExternInfo>> = RefCell::new(Vec::new());
    /// The running module's allocator bucket region base (from the
    /// `fai-dbg` heap metadata), for FAI_HEAP_VERIFY's host-side scan.
    /// Zero = unknown → scan disabled.
    static CURRENT_BUCKET_BASE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Per-run pointer tracker. Opaque integer handles are issued to
    /// the guest for every non-null pointer returned from C, so
    /// subsequent calls can re-materialise the pointer safely.
    static PTR_TRACKER: RefCell<PtrTracker> = RefCell::new(PtrTracker::new());
}

/// Install the extern metadata for an upcoming `run_wasm*`. Returned
/// guard clears the thread-local state on drop so the next run starts
/// clean.
pub struct ExternGuard;

impl ExternGuard {
    pub fn set(externs: Vec<ExternInfo>) -> Self {
        CURRENT_EXTERNS.with(|slot| *slot.borrow_mut() = externs);
        PTR_TRACKER.with(|slot| *slot.borrow_mut() = PtrTracker::new());
        ExternGuard
    }
}

impl Drop for ExternGuard {
    fn drop(&mut self) {
        CURRENT_EXTERNS.with(|slot| slot.borrow_mut().clear());
    }
}

/// Decode a NaN-boxed i64 from the guest into a host-side `Value` that
/// `fai_ffi::call` can consume. The marshaller dereferences object
/// pointers as host pointers, so any object reference we pass must live
/// on the host GC heap — not in wasm linear memory. For `String` args
/// we walk the guest's memory to read the bytes, then allocate a
/// host-side String object. For other arg types (word-sized primitives,
/// tracked `Ptr` handles) nothing needs to cross over — we just wrap
/// the bits.
fn decode_wasm_value_for_ffi(bits: u64, ty: &FfiType, guest_mem: &[u8]) -> fai_core::value::Value {
    // Null / primitive / handle paths: pass through. `fai_ffi::marshal`
    // inspects `is_null`, `is_int`, etc. on the resulting Value.
    let is_object = (bits & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT);
    if !is_object {
        return fai_core::value::Value::from_bits(bits);
    }

    match ty {
        FfiType::String => {
            let addr = (bits & ADDR_MASK) as usize;
            // String layout in linear memory: [tag:i32][len:i32][bytes..]
            if addr + 8 > guest_mem.len() {
                return fai_core::value::Value::null();
            }
            let tag = i32::from_le_bytes([
                guest_mem[addr],
                guest_mem[addr + 1],
                guest_mem[addr + 2],
                guest_mem[addr + 3],
            ]);
            if tag != OBJ_TAG_STRING {
                return fai_core::value::Value::null();
            }
            let len = i32::from_le_bytes([
                guest_mem[addr + 4],
                guest_mem[addr + 5],
                guest_mem[addr + 6],
                guest_mem[addr + 7],
            ]) as usize;
            let start = addr + 8;
            if start + len > guest_mem.len() {
                return fai_core::value::Value::null();
            }
            let bytes = &guest_mem[start..start + len];
            let s = String::from_utf8_lossy(bytes).into_owned();
            use fai_core::gc::{FaiString, GcHeader, GcRef, ObjType, Object, ObjectData};
            let obj = Box::new(Object {
                header: GcHeader {
                    rc: std::cell::Cell::new(1),
                    obj_type: ObjType::String,
                },
                data: ObjectData::String(FaiString { hash: 0, data: s }),
            });
            fai_core::value::Value::object(unsafe { GcRef::from_ptr(Box::into_raw(obj)) })
        }
        // Other FfiTypes don't marshal object references from the guest
        // heap (Pointer expects an Int handle, OutPtr ignores the
        // value, numeric types aren't objects). If someone passes an
        // object where a primitive is expected, let fai-ffi surface the
        // type mismatch rather than silently corrupting it.
        _ => fai_core::value::Value::from_bits(bits),
    }
}

/// Encode an FFI call's return `Value` into the bit pattern the guest
/// expects. Primitives (Int, Bool, Double, tracked Ptr handles, Null,
/// Void) are NaN-boxed the same way in host and guest, so their
/// `to_bits()` can pass straight through. `String` is the odd one out:
/// the host stores it as an `Object` pointer into the host GC heap,
/// but the guest's strings live in wasm linear memory with a
/// `[tag][len][bytes...]` layout. We allocate into the guest heap
/// using `wasm_alloc_str` and return a NaN-boxed guest-heap ref.
fn encode_return_for_guest(
    val: &fai_core::value::Value,
    ty: &FfiType,
    caller: &mut Caller<'_, ()>,
    mem: &Memory,
) -> i64 {
    if matches!(ty, FfiType::String) {
        if val.is_null() {
            return VAL_NULL;
        }
        if val.is_object() {
            if let fai_core::gc::ObjectData::String(s) = val.as_object().data() {
                return super::super::heap::wasm_alloc_str(caller, mem, &s.data);
            }
        }
    }
    val.to_bits() as i64
}

/// FAI_HEAP_VERIFY: host-side mirror of the guest allocator's bucket
/// scan. Reads every free-bucket head out of guest memory and reports
/// the first implausible or non-poisoned node. Run before/after an FFI
/// dispatch, a "clean before, dirty after" result convicts that extern
/// call of writing guest memory it doesn't own.
fn freelist_scan(caller: &mut Caller<'_, ()>, mem: &Memory) -> Option<String> {
    const NUM_BUCKETS: usize = 1024;
    const OBJ_TAG_POISON: i32 = 0x7E_DEAD;
    let bucket_base = CURRENT_BUCKET_BASE.with(|b| b.get()) as usize;
    if bucket_base == 0 {
        return None; // module carries no fai-dbg heap metadata
    }
    let heap_ptr = caller
        .get_export("__heap_ptr")
        .and_then(|e| e.into_global())
        .map(|g| g.get(&mut *caller).unwrap_i32() as u32)?;
    let data = mem.data(&caller);
    let read = |addr: usize| -> Option<i32> {
        Some(i32::from_le_bytes(data.get(addr..addr + 4)?.try_into().ok()?))
    };
    for idx in 0..NUM_BUCKETS {
        let node = read(bucket_base + idx * 4)? as u32;
        if node == 0 {
            continue;
        }
        if node % 8 != 0 || node < (bucket_base + NUM_BUCKETS * 4) as u32 || node >= heap_ptr {
            return Some(format!(
                "bucket[{}] head 0x{:x} out of range (heap_ptr 0x{:x})",
                idx, node, heap_ptr
            ));
        }
        let tag = read(node as usize + 8)?;
        if tag != OBJ_TAG_POISON {
            return Some(format!(
                "bucket[{}] head 0x{:x} tag word 0x{:x} (expected poison)",
                idx, node, tag
            ));
        }
    }
    None
}

/// Install the running module's bucket-region base for the host-side
/// FAI_HEAP_VERIFY scan (from the `fai-dbg` heap metadata).
pub(crate) fn set_bucket_base(base: u32) {
    CURRENT_BUCKET_BASE.with(|b| b.set(base));
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.call_ffi(ext_fn_idx: i32, arg_count: i32, args_ptr: i32) -> i64
    // Reads `arg_count` boxed Values from `args_ptr` in linear memory,
    // looks up the extern's C signature from the thread-local
    // `CURRENT_EXTERNS`, and dispatches via `fai_ffi::call`. Returns the
    // boxed result, or `VAL_NULL` if the library/symbol resolution or
    // argument unboxing fails — same shape a missing FFI path had when
    // this was a stub.
    linker
        .func_wrap(
            "env",
            "call_ffi",
            |mut caller: Caller<'_, ()>, ext_fn_idx: i32, arg_count: i32, args_ptr: i32| -> i64 {
                let info = match CURRENT_EXTERNS
                    .with(|slot| slot.borrow().get(ext_fn_idx as usize).cloned())
                {
                    Some(info) => info,
                    None => return VAL_NULL,
                };

                // Read `arg_count` boxed i64 values from guest memory.
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return VAL_NULL,
                };

                let heap_verify = std::env::var_os("FAI_HEAP_VERIFY").is_some();
                let dirty_before = if heap_verify {
                    freelist_scan(&mut caller, &mem)
                } else {
                    None
                };
                let mut raw_args: Vec<i64> = Vec::with_capacity(arg_count as usize);
                {
                    let data = mem.data(&caller);
                    for i in 0..arg_count as usize {
                        let off = args_ptr as usize + i * 8;
                        if off + 8 > data.len() {
                            return VAL_NULL;
                        }
                        let mut buf = [0u8; 8];
                        buf.copy_from_slice(&data[off..off + 8]);
                        raw_args.push(i64::from_le_bytes(buf));
                    }
                }

                // Convert raw i64s to fai_core::Value. String args are
                // NaN-boxed object refs pointing into the guest's linear
                // memory — we must decode them into host-side `Value`s
                // before handing them to `fai_ffi::call`, which
                // dereferences object pointers as host pointers. Other
                // param types (Int, Bool, Double, Pointer handles, Null,
                // OutPtr) are word-sized or don't touch the heap, so
                // pass through.
                let data_snapshot = mem.data(&caller).to_vec();
                let values: Vec<fai_core::value::Value> = raw_args
                    .iter()
                    .zip(
                        info.param_types
                            .iter()
                            .chain(std::iter::repeat(&FfiType::Int)),
                    )
                    .map(|(&bits, ty)| decode_wasm_value_for_ffi(bits as u64, ty, &data_snapshot))
                    .collect();

                // The extern's declared `param_types.len()` is the
                // "fixed" arg count; anything past that is variadic.
                let fixed = info.param_types.len();
                let supplied = values.len();

                let result = PTR_TRACKER.with(|tracker| {
                    let mut tracker = tracker.borrow_mut();
                    if supplied > fixed {
                        fai_ffi::call_variadic(
                            &info.library,
                            &info.function,
                            &values,
                            &info.param_types,
                            &info.return_type,
                            &mut tracker,
                            fixed,
                        )
                    } else {
                        fai_ffi::call(
                            &info.library,
                            &info.function,
                            &values,
                            &info.param_types,
                            &info.return_type,
                            &mut tracker,
                        )
                    }
                });

                if heap_verify && dirty_before.is_none() {
                    if let Some(dirt) = freelist_scan(&mut caller, &mem) {
                        eprintln!(
                            "[heap-verify] guest free list dirtied DURING FFI call \
                             {}::{} — {}",
                            info.library, info.function, dirt
                        );
                    }
                }

                match result {
                    Ok(call_result) => {
                        // Write any OutPtr results back into the
                        // guest's args buffer so the caller can read
                        // them via the same slot.
                        for (arg_idx, out_val) in &call_result.out_values {
                            let off = args_ptr as usize + arg_idx * 8;
                            let bits = out_val.to_bits() as i64;
                            let data = mem.data_mut(&mut caller);
                            if off + 8 <= data.len() {
                                data[off..off + 8].copy_from_slice(&bits.to_le_bytes());
                            }
                        }
                        // Marshal the return value back into a form the
                        // guest can use. For `String` returns the
                        // `Value` points at host-heap memory — we must
                        // copy the bytes into wasm linear memory and
                        // return a guest-heap NaN-box.
                        encode_return_for_guest(
                            &call_result.return_value,
                            &info.return_type,
                            &mut caller,
                            &mem,
                        )
                    }
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.ffi_begin(task_id, ext_fn_idx, arg_count, args_ptr) -> ()
    // The async (suspending) FFI path (plan 101 U7-U9): offload the blocking C
    // call to a boundary worker and leave the task parked. The codegen only
    // emits this for scalar-signature externs (Int/Float/Bool, no Ptr/String/
    // out-params), so the worker can build its `Value`s locally and hand back
    // just the NaN-boxed result bits — no guest memory, Store, or PtrTracker
    // crosses the thread boundary. The driver loop pumps the completion and
    // resumes the task; the guest then reads the value with `ffi_result`.
    linker
        .func_wrap(
            "env",
            "ffi_begin",
            |mut caller: Caller<'_, ()>, task_id: i32, ext_fn_idx: i32, arg_count: i32, args_ptr: i32| {
                let info = match CURRENT_EXTERNS
                    .with(|slot| slot.borrow().get(ext_fn_idx as usize).cloned())
                {
                    Some(info) => info,
                    None => return,
                };
                let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                    return;
                };
                let mut raw_args: Vec<i64> = Vec::with_capacity(arg_count as usize);
                {
                    let data = mem.data(&caller);
                    for i in 0..arg_count as usize {
                        let off = args_ptr as usize + i * 8;
                        if off + 8 > data.len() {
                            return;
                        }
                        let mut buf = [0u8; 8];
                        buf.copy_from_slice(&data[off..off + 8]);
                        raw_args.push(i64::from_le_bytes(buf));
                    }
                }
                super::boundary::with_boundary(|b| {
                    b.submit(task_id, move || {
                        // Scalar args don't read guest memory, so decode against
                        // an empty slice on this worker thread.
                        let values: Vec<fai_core::value::Value> = raw_args
                            .iter()
                            .zip(info.param_types.iter().chain(std::iter::repeat(&FfiType::Int)))
                            .map(|(&bits, ty)| decode_wasm_value_for_ffi(bits as u64, ty, &[]))
                            .collect();
                        let fixed = info.param_types.len();
                        let supplied = values.len();
                        let mut tracker = PtrTracker::new();
                        let result = if supplied > fixed {
                            fai_ffi::call_variadic(
                                &info.library,
                                &info.function,
                                &values,
                                &info.param_types,
                                &info.return_type,
                                &mut tracker,
                                fixed,
                            )
                        } else {
                            fai_ffi::call(
                                &info.library,
                                &info.function,
                                &values,
                                &info.param_types,
                                &info.return_type,
                                &mut tracker,
                            )
                        };
                        let bits: i64 = match result {
                            Ok(call_result) => call_result.return_value.to_bits() as i64,
                            Err(_) => VAL_NULL,
                        };
                        Box::new(bits) as Box<dyn std::any::Any + Send>
                    });
                });
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.ffi_result(task_id) -> i64 — the NaN-boxed result the worker computed
    // for the offloaded extern call (or null if it failed / went missing).
    linker
        .func_wrap("env", "ffi_result", |_caller: Caller<'_, ()>, task_id: i32| -> i64 {
            match super::boundary::take_ready(task_id) {
                Some(Ok(boxed)) => boxed.downcast::<i64>().map(|b| *b).unwrap_or(VAL_NULL),
                _ => VAL_NULL,
            }
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // env.float_to_str(value: f64, buf_ptr: i32) -> i32 (length)
    linker
        .func_wrap(
            "env",
            "float_to_str",
            |mut caller: Caller<'_, ()>, val: f64, buf_ptr: i32| -> i32 {
                let s = format_float(val);
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let bytes = s.as_bytes();
                mem.data_mut(&mut caller)[buf_ptr as usize..buf_ptr as usize + bytes.len()]
                    .copy_from_slice(bytes);
                bytes.len() as i32
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}
