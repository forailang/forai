//! Marshaling between FAI `Value` and C types.
//!
//! Uses typed `extern "C" fn` pointer transmutes instead of libffi.
//! This works correctly on all platforms including aarch64 macOS.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

use fai_core::gc::*;
use fai_core::value::Value;

use crate::ptr_tracker::PtrTracker;

/// C types that can cross the FFI boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum FfiType {
    Int,
    Double,
    String,
    Bool,
    Pointer,
    Void,
    OutPtr,
}

/// Holds marshaled C arguments and their temporary storage.
pub struct MarshaledArgs {
    /// Each arg is either a word (usize) or float (f64), in call order.
    args: Vec<CArg>,
    /// Temporary CStrings kept alive for the duration of the call.
    _temp_strings: Vec<CString>,
    /// Scratch slots for OutPtr parameters.
    pub out_ptr_slots: Vec<*mut c_void>,
    /// Maps slot_idx → arg_idx.
    pub out_ptr_arg_indices: Vec<usize>,
}

#[derive(Clone, Copy)]
enum CArg {
    Word(usize),
    Float(f64),
}

impl MarshaledArgs {
    pub fn new() -> Self {
        Self {
            args: Vec::new(),
            _temp_strings: Vec::new(),
            out_ptr_slots: Vec::new(),
            out_ptr_arg_indices: Vec::new(),
        }
    }

    pub fn push(
        &mut self,
        val: &Value,
        ty: &FfiType,
        ptr_tracker: &PtrTracker,
    ) -> Result<(), std::string::String> {
        match ty {
            FfiType::Int => {
                if val.is_int() {
                    self.args.push(CArg::Word(val.as_int() as u32 as usize));
                } else if val.is_float() {
                    self.args
                        .push(CArg::Word(val.as_float() as i32 as u32 as usize));
                } else {
                    return Err("expected Int value".into());
                }
            }
            FfiType::Double => {
                if val.is_float() {
                    self.args.push(CArg::Float(val.as_float()));
                } else if val.is_int() {
                    self.args.push(CArg::Float(val.as_int() as f64));
                } else {
                    return Err("expected Float value".into());
                }
            }
            FfiType::String => {
                if val.is_null() {
                    self.args.push(CArg::Word(0));
                } else if val.is_object() {
                    if let ObjectData::String(s) = val.as_object().data() {
                        let cstr = CString::new(s.data.as_str())
                            .map_err(|_| "string contains null byte".to_string())?;
                        // Use into_raw() so the allocation persists beyond this call.
                        // C functions like sqlite3_bind_text with SQLITE_STATIC require
                        // the string to remain valid until the prepared statement is
                        // finalized — which is longer than a single FFI call frame.
                        let ptr = cstr.into_raw() as usize;
                        self.args.push(CArg::Word(ptr));
                    } else {
                        return Err("expected String value".into());
                    }
                } else {
                    return Err("expected String value".into());
                }
            }
            FfiType::Bool => {
                if val.is_bool() {
                    self.args.push(CArg::Word(val.as_bool() as usize));
                } else {
                    return Err("expected Bool value".into());
                }
            }
            FfiType::Pointer => {
                if val.is_null() {
                    self.args.push(CArg::Word(0));
                } else if val.is_int() {
                    let handle = val.as_int() as u32;
                    let raw = ptr_tracker
                        .get(handle)
                        .ok_or_else(|| format!("invalid Ptr handle {}", handle))?;
                    self.args.push(CArg::Word(raw as usize));
                } else {
                    return Err("expected Ptr value".into());
                }
            }
            FfiType::OutPtr => {
                let arg_idx = self.args.len();
                let slot_idx = self.out_ptr_slots.len();
                self.out_ptr_slots.push(std::ptr::null_mut());
                self.out_ptr_arg_indices.push(arg_idx);
                // Pass the address of the scratch slot
                let slot_ptr = &self.out_ptr_slots[slot_idx] as *const *mut c_void as usize;
                self.args.push(CArg::Word(slot_ptr));
            }
            FfiType::Void => {
                return Err("cannot pass Void as argument".into());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.args.len()
    }

    /// Re-resolve OutPtr slot addresses (call after all pushes, before ffi_call).
    /// Needed because Vec may have reallocated between pushes.
    pub fn fixup_out_ptrs(&mut self) {
        for (i, &arg_idx) in self.out_ptr_arg_indices.iter().enumerate() {
            let slot_ptr = &self.out_ptr_slots[i] as *const *mut c_void as usize;
            self.args[arg_idx] = CArg::Word(slot_ptr);
        }
    }
}

// ── Return value conversion ─────────────────────────────────────────

fn convert_int_return(raw: usize) -> Value {
    Value::int(raw as i32)
}

fn convert_bool_return(raw: usize) -> Value {
    Value::bool(raw as i32 != 0)
}

fn convert_float_return(raw: f64) -> Value {
    Value::float(raw)
}

unsafe fn convert_string_return(raw: usize) -> Value {
    let ptr = raw as *const c_char;
    if ptr.is_null() {
        Value::null()
    } else {
        let cstr = CStr::from_ptr(ptr);
        let s = cstr.to_string_lossy().into_owned();
        let obj = Box::new(Object {
            header: GcHeader {
                rc: std::cell::Cell::new(1),
                obj_type: ObjType::String,
            },
            data: ObjectData::String(FaiString { hash: 0, data: s }),
        });
        Value::object(GcRef::from_ptr(Box::into_raw(obj)))
    }
}

fn convert_pointer_return(raw: usize, ptr_tracker: &mut PtrTracker) -> Value {
    let ptr = raw as *mut c_void;
    if ptr.is_null() {
        Value::null()
    } else {
        let handle = ptr_tracker.track(ptr);
        Value::int(handle as i32)
    }
}

fn convert_word_return(raw: usize, return_type: &FfiType, ptr_tracker: &mut PtrTracker) -> Value {
    match return_type {
        FfiType::Int => convert_int_return(raw),
        FfiType::Bool => convert_bool_return(raw),
        FfiType::String => unsafe { convert_string_return(raw) },
        FfiType::Pointer => convert_pointer_return(raw, ptr_tracker),
        _ => convert_int_return(raw),
    }
}

// ── Typed dispatch ──────────────────────────────────────────────────

/// Perform an FFI call using typed `extern "C" fn` transmutes.
///
/// # Safety
/// Caller must ensure fn_ptr is a valid function pointer and args match the
/// function's actual C signature.
pub unsafe fn ffi_call(
    fn_ptr: *const c_void,
    args: &mut MarshaledArgs,
    return_type: &FfiType,
    ptr_tracker: &mut PtrTracker,
    _fixed_arg_count: Option<usize>, // kept for API compat, not needed for transmute
) -> Result<Value, std::string::String> {
    // Fix up OutPtr slot addresses in case Vec reallocated
    args.fixup_out_ptrs();

    // Check if any arg is a float (needs separate register on aarch64)
    let has_float = args.args.iter().any(|a| matches!(a, CArg::Float(_)));

    if has_float {
        ffi_call_with_floats(fn_ptr, args, return_type, ptr_tracker)
    } else {
        ffi_call_all_words(fn_ptr, args, return_type, ptr_tracker)
    }
}

/// Fast path: all args are word-sized (i32, pointers, etc.)
/// This covers the vast majority of C API calls.
unsafe fn ffi_call_all_words(
    fn_ptr: *const c_void,
    args: &MarshaledArgs,
    return_type: &FfiType,
    ptr_tracker: &mut PtrTracker,
) -> Result<Value, std::string::String> {
    // Extract word args
    let w: Vec<usize> = args
        .args
        .iter()
        .map(|a| match a {
            CArg::Word(v) => *v,
            CArg::Float(_) => unreachable!(),
        })
        .collect();

    // Returns usize for int/bool/string/pointer, or call void variant
    let returns_float = matches!(return_type, FfiType::Double);

    if returns_float {
        let raw: f64 = match w.len() {
            0 => {
                let f: extern "C" fn() -> f64 = std::mem::transmute(fn_ptr);
                f()
            }
            1 => {
                let f: extern "C" fn(usize) -> f64 = std::mem::transmute(fn_ptr);
                f(w[0])
            }
            2 => {
                let f: extern "C" fn(usize, usize) -> f64 = std::mem::transmute(fn_ptr);
                f(w[0], w[1])
            }
            _ => {
                return Err(format!(
                    "unsupported arg count {} for float return",
                    w.len()
                ))
            }
        };
        return Ok(convert_float_return(raw));
    }

    if matches!(return_type, FfiType::Void) {
        match w.len() {
            0 => {
                let f: extern "C" fn() = std::mem::transmute(fn_ptr);
                f()
            }
            1 => {
                let f: extern "C" fn(usize) = std::mem::transmute(fn_ptr);
                f(w[0])
            }
            2 => {
                let f: extern "C" fn(usize, usize) = std::mem::transmute(fn_ptr);
                f(w[0], w[1])
            }
            3 => {
                let f: extern "C" fn(usize, usize, usize) = std::mem::transmute(fn_ptr);
                f(w[0], w[1], w[2])
            }
            4 => {
                let f: extern "C" fn(usize, usize, usize, usize) = std::mem::transmute(fn_ptr);
                f(w[0], w[1], w[2], w[3])
            }
            5 => {
                let f: extern "C" fn(usize, usize, usize, usize, usize) =
                    std::mem::transmute(fn_ptr);
                f(w[0], w[1], w[2], w[3], w[4])
            }
            6 => {
                let f: extern "C" fn(usize, usize, usize, usize, usize, usize) =
                    std::mem::transmute(fn_ptr);
                f(w[0], w[1], w[2], w[3], w[4], w[5])
            }
            7 => {
                let f: extern "C" fn(usize, usize, usize, usize, usize, usize, usize) =
                    std::mem::transmute(fn_ptr);
                f(w[0], w[1], w[2], w[3], w[4], w[5], w[6])
            }
            8 => {
                let f: extern "C" fn(usize, usize, usize, usize, usize, usize, usize, usize) =
                    std::mem::transmute(fn_ptr);
                f(w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7])
            }
            n => return Err(format!("unsupported arg count {} (max 8)", n)),
        };
        return Ok(Value::void());
    }

    // Returns usize (covers Int, Bool, String, Pointer)
    let raw: usize = match w.len() {
        0 => {
            let f: extern "C" fn() -> usize = std::mem::transmute(fn_ptr);
            f()
        }
        1 => {
            let f: extern "C" fn(usize) -> usize = std::mem::transmute(fn_ptr);
            f(w[0])
        }
        2 => {
            let f: extern "C" fn(usize, usize) -> usize = std::mem::transmute(fn_ptr);
            f(w[0], w[1])
        }
        3 => {
            let f: extern "C" fn(usize, usize, usize) -> usize = std::mem::transmute(fn_ptr);
            f(w[0], w[1], w[2])
        }
        4 => {
            let f: extern "C" fn(usize, usize, usize, usize) -> usize = std::mem::transmute(fn_ptr);
            f(w[0], w[1], w[2], w[3])
        }
        5 => {
            let f: extern "C" fn(usize, usize, usize, usize, usize) -> usize =
                std::mem::transmute(fn_ptr);
            f(w[0], w[1], w[2], w[3], w[4])
        }
        6 => {
            let f: extern "C" fn(usize, usize, usize, usize, usize, usize) -> usize =
                std::mem::transmute(fn_ptr);
            f(w[0], w[1], w[2], w[3], w[4], w[5])
        }
        7 => {
            let f: extern "C" fn(usize, usize, usize, usize, usize, usize, usize) -> usize =
                std::mem::transmute(fn_ptr);
            f(w[0], w[1], w[2], w[3], w[4], w[5], w[6])
        }
        8 => {
            let f: extern "C" fn(usize, usize, usize, usize, usize, usize, usize, usize) -> usize =
                std::mem::transmute(fn_ptr);
            f(w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7])
        }
        n => return Err(format!("unsupported arg count {} (max 8)", n)),
    };

    Ok(convert_word_return(raw, return_type, ptr_tracker))
}

/// Slow path: at least one float arg. We handle the most common patterns:
/// one float arg at various positions, with the rest being words.
unsafe fn ffi_call_with_floats(
    fn_ptr: *const c_void,
    args: &MarshaledArgs,
    return_type: &FfiType,
    ptr_tracker: &mut PtrTracker,
) -> Result<Value, std::string::String> {
    // For float-containing signatures, handle common patterns.
    // Collect args as (position, is_float) and dispatch.
    let n = args.args.len();

    // Single float arg, no other args → common for math functions
    if n == 1 {
        if let CArg::Float(f) = args.args[0] {
            if matches!(return_type, FfiType::Double) {
                let func: extern "C" fn(f64) -> f64 = std::mem::transmute(fn_ptr);
                return Ok(convert_float_return(func(f)));
            } else {
                let func: extern "C" fn(f64) -> usize = std::mem::transmute(fn_ptr);
                let raw = func(f);
                return Ok(convert_word_return(raw, return_type, ptr_tracker));
            }
        }
    }

    // Two args with floats
    if n == 2 {
        let (a0, a1) = (args.args[0], args.args[1]);
        match (a0, a1) {
            (CArg::Float(f0), CArg::Float(f1)) => {
                if matches!(return_type, FfiType::Double) {
                    let func: extern "C" fn(f64, f64) -> f64 = std::mem::transmute(fn_ptr);
                    return Ok(convert_float_return(func(f0, f1)));
                } else {
                    let func: extern "C" fn(f64, f64) -> usize = std::mem::transmute(fn_ptr);
                    return Ok(convert_word_return(func(f0, f1), return_type, ptr_tracker));
                }
            }
            (CArg::Word(w), CArg::Float(f)) => {
                if matches!(return_type, FfiType::Double) {
                    let func: extern "C" fn(usize, f64) -> f64 = std::mem::transmute(fn_ptr);
                    return Ok(convert_float_return(func(w, f)));
                } else {
                    let func: extern "C" fn(usize, f64) -> usize = std::mem::transmute(fn_ptr);
                    return Ok(convert_word_return(func(w, f), return_type, ptr_tracker));
                }
            }
            (CArg::Float(f), CArg::Word(w)) => {
                if matches!(return_type, FfiType::Double) {
                    let func: extern "C" fn(f64, usize) -> f64 = std::mem::transmute(fn_ptr);
                    return Ok(convert_float_return(func(f, w)));
                } else {
                    let func: extern "C" fn(f64, usize) -> usize = std::mem::transmute(fn_ptr);
                    return Ok(convert_word_return(func(f, w), return_type, ptr_tracker));
                }
            }
            _ => {} // both words — shouldn't reach here
        }
    }

    // Three-arg patterns with one float
    if n == 3 {
        let (a0, a1, a2) = (args.args[0], args.args[1], args.args[2]);
        let raw: usize = match (a0, a1, a2) {
            (CArg::Float(f0), CArg::Float(f1), CArg::Float(f2)) => {
                if matches!(return_type, FfiType::Double) {
                    let func: extern "C" fn(f64, f64, f64) -> f64 = std::mem::transmute(fn_ptr);
                    return Ok(convert_float_return(func(f0, f1, f2)));
                }
                let func: extern "C" fn(f64, f64, f64) -> usize = std::mem::transmute(fn_ptr);
                func(f0, f1, f2)
            }
            (CArg::Word(w0), CArg::Word(w1), CArg::Float(f2)) => {
                let func: extern "C" fn(usize, usize, f64) -> usize = std::mem::transmute(fn_ptr);
                func(w0, w1, f2)
            }
            (CArg::Word(w0), CArg::Float(f1), CArg::Word(w2)) => {
                let func: extern "C" fn(usize, f64, usize) -> usize = std::mem::transmute(fn_ptr);
                func(w0, f1, w2)
            }
            (CArg::Float(f0), CArg::Word(w1), CArg::Word(w2)) => {
                let func: extern "C" fn(f64, usize, usize) -> usize = std::mem::transmute(fn_ptr);
                func(f0, w1, w2)
            }
            (CArg::Word(w0), CArg::Float(f1), CArg::Float(f2)) => {
                let func: extern "C" fn(usize, f64, f64) -> usize = std::mem::transmute(fn_ptr);
                func(w0, f1, f2)
            }
            (CArg::Float(f0), CArg::Word(w1), CArg::Float(f2)) => {
                let func: extern "C" fn(f64, usize, f64) -> usize = std::mem::transmute(fn_ptr);
                func(f0, w1, f2)
            }
            (CArg::Float(f0), CArg::Float(f1), CArg::Word(w2)) => {
                let func: extern "C" fn(f64, f64, usize) -> usize = std::mem::transmute(fn_ptr);
                func(f0, f1, w2)
            }
            _ => return Err("unsupported FFI 3-arg pattern".to_string()),
        };
        return Ok(convert_word_return(raw, return_type, ptr_tracker));
    }

    Err(format!(
        "unsupported FFI signature: {} args with mixed int/float types",
        n
    ))
}
