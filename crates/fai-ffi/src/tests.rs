//! Round-trip marshaling tests for the FFI bridge.
//!
//! Uses standard C library functions available on all platforms.

use crate::{call, FfiType, PtrTracker};
use fai_core::gc::*;
use fai_core::value::Value;

/// Helper: create a FAI string value.
fn fai_string(s: &str) -> Value {
    let obj = Box::new(Object {
        header: GcHeader {
            rc: std::cell::Cell::new(1),
            obj_type: ObjType::String,
        },
        data: ObjectData::String(FaiString {
            hash: 0,
            data: s.to_string(),
        }),
    });
    Value::object(unsafe { GcRef::from_ptr(Box::into_raw(obj)) })
}

// The C library name varies by platform
#[cfg(target_os = "macos")]
const LIBC: &str = "System";
#[cfg(target_os = "linux")]
const LIBC: &str = "c";

// ── Int round-trip ──────────────────────────────────────────────────

#[test]
fn test_int_abs() {
    // C: int abs(int) — returns absolute value
    let mut tracker = PtrTracker::new();
    let result = call(
        LIBC,
        "abs",
        &[Value::int(-42)],
        &[FfiType::Int],
        &FfiType::Int,
        &mut tracker,
    )
    .unwrap()
    .return_value;
    assert!(result.is_int());
    assert_eq!(result.as_int(), 42);
}

#[test]
fn test_int_abs_positive() {
    let mut tracker = PtrTracker::new();
    let result = call(
        LIBC,
        "abs",
        &[Value::int(7)],
        &[FfiType::Int],
        &FfiType::Int,
        &mut tracker,
    )
    .unwrap()
    .return_value;
    assert_eq!(result.as_int(), 7);
}

// ── String → Int ────────────────────────────────────────────────────

#[test]
fn test_string_to_int_atoi() {
    // C: int atoi(const char*) — parses string to int
    let mut tracker = PtrTracker::new();
    let result = call(
        LIBC,
        "atoi",
        &[fai_string("12345")],
        &[FfiType::String],
        &FfiType::Int,
        &mut tracker,
    )
    .unwrap()
    .return_value;
    assert_eq!(result.as_int(), 12345);
}

#[test]
fn test_atoi_negative() {
    let mut tracker = PtrTracker::new();
    let result = call(
        LIBC,
        "atoi",
        &[fai_string("-99")],
        &[FfiType::String],
        &FfiType::Int,
        &mut tracker,
    )
    .unwrap()
    .return_value;
    assert_eq!(result.as_int(), -99);
}

// ── String → String (strlen as Int) ────────────────────────────────

#[test]
fn test_strlen() {
    // C: size_t strlen(const char*) — but we treat as int return
    // On most platforms size_t fits in int for short strings
    let mut tracker = PtrTracker::new();
    let result = call(
        LIBC,
        "strlen",
        &[fai_string("hello")],
        &[FfiType::String],
        &FfiType::Int,
        &mut tracker,
    )
    .unwrap()
    .return_value;
    assert_eq!(result.as_int(), 5);
}

#[test]
fn test_strlen_empty() {
    let mut tracker = PtrTracker::new();
    let result = call(
        LIBC,
        "strlen",
        &[fai_string("")],
        &[FfiType::String],
        &FfiType::Int,
        &mut tracker,
    )
    .unwrap()
    .return_value;
    assert_eq!(result.as_int(), 0);
}

// ── Double round-trip ───────────────────────────────────────────────

#[test]
fn test_double_floor() {
    // C: double floor(double)
    let lib = if cfg!(target_os = "macos") {
        "System"
    } else {
        "m"
    };
    let mut tracker = PtrTracker::new();
    let result = call(
        lib,
        "floor",
        &[Value::float(3.7)],
        &[FfiType::Double],
        &FfiType::Double,
        &mut tracker,
    )
    .unwrap()
    .return_value;
    assert!(result.is_float());
    assert_eq!(result.as_float(), 3.0);
}

#[test]
fn test_double_ceil() {
    let lib = if cfg!(target_os = "macos") {
        "System"
    } else {
        "m"
    };
    let mut tracker = PtrTracker::new();
    let result = call(
        lib,
        "ceil",
        &[Value::float(3.2)],
        &[FfiType::Double],
        &FfiType::Double,
        &mut tracker,
    )
    .unwrap()
    .return_value;
    assert_eq!(result.as_float(), 4.0);
}

#[test]
fn test_double_sqrt() {
    let lib = if cfg!(target_os = "macos") {
        "System"
    } else {
        "m"
    };
    let mut tracker = PtrTracker::new();
    let result = call(
        lib,
        "sqrt",
        &[Value::float(16.0)],
        &[FfiType::Double],
        &FfiType::Double,
        &mut tracker,
    )
    .unwrap()
    .return_value;
    assert_eq!(result.as_float(), 4.0);
}

// ── Ptr lifecycle ───────────────────────────────────────────────────

#[test]
fn test_ptr_track_and_release() {
    let mut tracker = PtrTracker::new();
    let h1 = tracker.track(0xABCD as *mut std::os::raw::c_void);
    assert!(tracker.is_valid(h1));
    let ptr = tracker.release(h1);
    assert!(ptr.is_some());
    assert!(!tracker.is_valid(h1));
}

// ── Error handling ──────────────────────────────────────────────────

#[test]
fn test_wrong_arg_count() {
    let mut tracker = PtrTracker::new();
    let result = call(
        LIBC,
        "abs",
        &[Value::int(1), Value::int(2)],
        &[FfiType::Int],
        &FfiType::Int,
        &mut tracker,
    );
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .message
        .contains("expected 1 args, got 2"));
}

#[test]
fn test_library_not_found() {
    let mut tracker = PtrTracker::new();
    let result = call(
        "fai_nonexistent_lib_xyz",
        "foo",
        &[],
        &[],
        &FfiType::Void,
        &mut tracker,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("cannot load library"));
}

#[test]
fn test_symbol_not_found() {
    let mut tracker = PtrTracker::new();
    let result = call(
        LIBC,
        "fai_nonexistent_function_xyz",
        &[],
        &[],
        &FfiType::Void,
        &mut tracker,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("not found"));
}

#[test]
fn test_type_mismatch_int_expected() {
    let mut tracker = PtrTracker::new();
    let result = call(
        LIBC,
        "abs",
        &[Value::bool(true)],
        &[FfiType::Int],
        &FfiType::Int,
        &mut tracker,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().message.contains("expected Int"));
}

#[test]
fn test_outptr_marshaling() {
    // Verify OutPtr marshaling creates a scratch slot and builds correct arg layout
    use crate::marshal::{FfiType as Ft, MarshaledArgs};
    let tracker = PtrTracker::new();
    let mut args = MarshaledArgs::new();
    args.push(&fai_string("test"), &Ft::String, &tracker)
        .unwrap();
    args.push(&Value::null(), &Ft::OutPtr, &tracker).unwrap();
    assert_eq!(args.len(), 2);
    assert_eq!(args.out_ptr_slots.len(), 1);
    assert_eq!(args.out_ptr_arg_indices[0], 1);
}

#[test]
fn test_outptr_sqlite3_open() {
    // sqlite3_open(const char*, sqlite3**) -> int
    let mut tracker = PtrTracker::new();
    let result = call(
        "sqlite3",
        "sqlite3_open",
        &[fai_string(":memory:"), Value::null()],
        &[FfiType::String, FfiType::OutPtr],
        &FfiType::Int,
        &mut tracker,
    )
    .unwrap();
    assert_eq!(result.return_value.as_int(), 0); // SQLITE_OK
    assert_eq!(result.out_values.len(), 1);
    assert!(
        !result.out_values[0].1.is_null(),
        "db handle should not be null"
    );

    // Close it via the handle
    let db_handle = result.out_values[0].1;
    let close_result = call(
        "sqlite3",
        "sqlite3_close",
        &[db_handle],
        &[FfiType::Pointer],
        &FfiType::Int,
        &mut tracker,
    )
    .unwrap();
    assert_eq!(close_result.return_value.as_int(), 0);
}

#[test]
fn test_null_string_marshals_to_null_ptr() {
    // Verify null FAI value marshals to a null C pointer without calling a C function
    use crate::marshal::{FfiType as Ft, MarshaledArgs};
    let tracker = PtrTracker::new();
    let mut args = MarshaledArgs::new();
    let result = args.push(&Value::null(), &Ft::String, &tracker);
    assert!(result.is_ok());
    assert_eq!(args.len(), 1);
}

// ── Regression: RTLD_GLOBAL exposes dependency symbols to extensions ──
//
// Loadable extensions dlopened *by* a loaded library (e.g. sqlite-vec via
// `sqlite3_load_extension`) must be able to resolve symbols from the host's
// dependency chain. libsqlite3 depends on libm; after the FFI bridge loads it,
// libm's `sqrtf` must be resolvable in the global symbol scope. With the old
// `RTLD_LOCAL` load it was hidden and such extensions failed with
// "undefined symbol: sqrtf". `get_library` now loads with `RTLD_GLOBAL`.
#[cfg(unix)]
#[test]
fn loaded_library_exposes_dependency_symbols_globally() {
    use libloading::os::unix::{Library, RTLD_NOW};

    // Force libsqlite3 (which depends on libm) through the FFI bridge.
    crate::get_library("sqlite3").expect("libsqlite3 should be loadable");

    // dlopen(NULL) yields a handle over the process-global symbol scope.
    let global = unsafe { Library::open(None::<&str>, RTLD_NOW) }.expect("global symbol handle");
    let sqrtf: Result<libloading::os::unix::Symbol<*const std::os::raw::c_void>, _> =
        unsafe { global.get(b"sqrtf\0") };
    assert!(
        sqrtf.is_ok(),
        "libm's sqrtf must be globally resolvable after loading libsqlite3 \
         (RTLD_GLOBAL regression — extensions like sqlite-vec depend on this)"
    );
}
