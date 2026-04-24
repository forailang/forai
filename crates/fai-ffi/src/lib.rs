//! FFI bridge for the FAI language.
//!
//! Provides `call(lib, func, args, param_types, return_type)` to invoke C functions
//! from FAI code. Called by the wasm runner's FFI host import.
//!
//! Architecture:
//! - Libraries loaded via `libloading` (dlopen/LoadLibrary), cached per name
//! - Functions looked up via dlsym, cached per (library, function) pair
//! - C calling convention via `libffi` for dynamic dispatch
//! - FAI `Value` ↔ C type marshaling at call boundaries

use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::Mutex;

use fai_core::value::Value;

mod marshal;
mod ptr_tracker;

pub use marshal::FfiType;
pub use ptr_tracker::PtrTracker;

// ── Error type ──────────────────────────────────────────────────────

#[derive(Debug)]
pub struct FfiError {
    pub message: String,
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FFI error: {}", self.message)
    }
}

// ── Library cache ───────────────────────────────────────────────────

/// Global cache of loaded shared libraries.
/// Key: library name (e.g. "sqlite3").
/// Uses libloading::Library which wraps dlopen handles.
static LIBRARY_CACHE: Mutex<Option<HashMap<String, &'static libloading::Library>>> =
    Mutex::new(None);

fn get_library(lib_name: &str) -> Result<&'static libloading::Library, FfiError> {
    let mut cache = LIBRARY_CACHE.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);

    if let Some(lib) = cache.get(lib_name) {
        return Ok(*lib);
    }

    // Try loading with standard library name patterns
    let candidates = if cfg!(target_os = "macos") {
        vec![
            format!("lib{}.dylib", lib_name),
            format!("lib{}.a", lib_name),
            lib_name.to_string(),
        ]
    } else if cfg!(target_os = "windows") {
        vec![format!("{}.dll", lib_name), lib_name.to_string()]
    } else {
        vec![
            format!("lib{}.so.6", lib_name),
            format!("lib{}.so", lib_name),
            lib_name.to_string(),
        ]
    };

    for candidate in &candidates {
        match unsafe { libloading::Library::new(candidate) } {
            Ok(lib) => {
                let lib = Box::leak(Box::new(lib));
                cache.insert(lib_name.to_string(), lib);
                return Ok(lib);
            }
            Err(_) => continue,
        }
    }

    Err(FfiError {
        message: format!("cannot load library '{}': not found", lib_name),
    })
}

// ── Function cache ──────────────────────────────────────────────────

/// Wrapper around a function pointer to make it Send+Sync.
/// Safety: function pointers from dlsym are process-global and safe to share.
struct FnPtr(*const c_void);
unsafe impl Send for FnPtr {}
unsafe impl Sync for FnPtr {}

/// Global cache of resolved function symbols.
/// Key: (library_name, function_name).
static FUNCTION_CACHE: Mutex<Option<HashMap<(String, String), FnPtr>>> = Mutex::new(None);

fn get_function(lib_name: &str, func_name: &str) -> Result<*const c_void, FfiError> {
    let key = (lib_name.to_string(), func_name.to_string());

    let mut cache = FUNCTION_CACHE.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);

    if let Some(ptr) = cache.get(&key) {
        return Ok(ptr.0);
    }

    let lib = get_library(lib_name)?;
    let symbol_name = CString::new(func_name).map_err(|_| FfiError {
        message: format!("invalid function name '{}'", func_name),
    })?;

    let raw_ptr: *const c_void = unsafe {
        let sym: libloading::Symbol<*const c_void> = lib
            .get(symbol_name.as_bytes_with_nul())
            .map_err(|e| FfiError {
                message: format!("symbol '{}' not found in '{}': {}", func_name, lib_name, e),
            })?;
        *sym
    };

    cache.insert(key, FnPtr(raw_ptr));
    Ok(raw_ptr)
}

// ── Public API ──────────────────────────────────────────────────────

/// Call a C function through the FFI bridge.
///
/// # Arguments
/// - `lib`: Library name (e.g. "sqlite3")
/// - `func`: Function name (e.g. "sqlite3_open")
/// - `args`: FAI values to pass as arguments
/// - `param_types`: Expected C types for each parameter
/// - `return_type`: Expected C return type
///
/// # Returns
/// The C function's return value marshaled back to a FAI `Value`.
pub fn call(
    lib: &str,
    func: &str,
    args: &[Value],
    param_types: &[FfiType],
    return_type: &FfiType,
    ptr_tracker: &mut PtrTracker,
) -> Result<CallResult, FfiError> {
    call_with_options(lib, func, args, param_types, return_type, ptr_tracker, None)
}

/// Call a variadic C function through the FFI bridge.
///
/// `fixed_arg_count` specifies how many leading arguments are fixed
/// (required for correct calling convention on aarch64).
pub fn call_variadic(
    lib: &str,
    func: &str,
    args: &[Value],
    param_types: &[FfiType],
    return_type: &FfiType,
    ptr_tracker: &mut PtrTracker,
    fixed_arg_count: usize,
) -> Result<CallResult, FfiError> {
    call_with_options(
        lib,
        func,
        args,
        param_types,
        return_type,
        ptr_tracker,
        Some(fixed_arg_count),
    )
}

fn call_with_options(
    lib: &str,
    func: &str,
    args: &[Value],
    param_types: &[FfiType],
    return_type: &FfiType,
    ptr_tracker: &mut PtrTracker,
    fixed_arg_count: Option<usize>,
) -> Result<CallResult, FfiError> {
    if args.len() != param_types.len() {
        return Err(FfiError {
            message: format!(
                "{}.{}: expected {} args, got {}",
                lib,
                func,
                param_types.len(),
                args.len()
            ),
        });
    }

    let fn_ptr = get_function(lib, func)?;

    // Marshal FAI values → C arguments
    let mut c_args = marshal::MarshaledArgs::new();
    for (i, (val, ty)) in args.iter().zip(param_types.iter()).enumerate() {
        c_args.push(val, ty, ptr_tracker).map_err(|e| FfiError {
            message: format!("{}.{} arg {}: {}", lib, func, i, e),
        })?;
    }

    // Build libffi argument types and call
    let result = unsafe {
        marshal::ffi_call(
            fn_ptr,
            &mut c_args,
            return_type,
            ptr_tracker,
            fixed_arg_count,
        )
    }
    .map_err(|e| FfiError {
        message: format!("{}.{}: {}", lib, func, e),
    })?;

    // Read back OutPtr values: track any non-null pointers written by C
    let mut out_values: Vec<(usize, Value)> = Vec::new();
    for (i, written_ptr) in c_args.out_ptr_slots.iter().enumerate() {
        let arg_idx = c_args.out_ptr_arg_indices[i];
        let written_ptr = *written_ptr;
        if written_ptr.is_null() {
            out_values.push((arg_idx, Value::null()));
        } else {
            let handle = ptr_tracker.track(written_ptr);
            out_values.push((arg_idx, Value::int(handle as i32)));
        }
    }

    Ok(CallResult {
        return_value: result,
        out_values,
    })
}

/// Result of an FFI call, including any output pointer values.
#[derive(Debug)]
pub struct CallResult {
    /// The C function's return value.
    pub return_value: Value,
    /// Output pointer values: (arg_index, tracked_handle_value).
    /// Empty if no `OutPtr` params were used.
    pub out_values: Vec<(usize, Value)>,
}

#[cfg(test)]
mod tests;
