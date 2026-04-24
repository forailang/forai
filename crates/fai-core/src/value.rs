//! NaN-boxed value representation for the FAI runtime.
//!
//! Every FAI value fits in a single u64. IEEE 754 doubles have a large
//! space of NaN bit patterns we exploit to tag non-float values:
//!
//!   Float:   raw f64 bits (any non-NaN or the canonical NaN)
//!   Int:     QNAN | TAG_INT | i32 payload
//!   Bool:    QNAN | TAG_BOOL | 0 or 1
//!   Null:    QNAN | TAG_NULL
//!   Void:    QNAN | TAG_VOID
//!   Object:  QNAN | SIGN_BIT | 48-bit pointer

use std::fmt;

use crate::gc::{GcRef, Object};

/// Quiet NaN with the Intel QNaN indefinite pattern.
/// Bits: 0_11111111111_1100000000...0 = 0x7FFC_0000_0000_0000
const QNAN: u64 = 0x7FFC_0000_0000_0000;

/// Sign bit, used to distinguish object pointers from tagged immediates.
const SIGN_BIT: u64 = 0x8000_0000_0000_0000;

// Tag bits (bits 48..50, inside the NaN payload)
const TAG_NULL: u64 = 0x0001_0000_0000_0000;
const TAG_VOID: u64 = 0x0002_0000_0000_0000;
const TAG_BOOL: u64 = 0x0003_0000_0000_0000;
const TAG_INT: u64 = 0x0004_0000_0000_0000;

/// A NaN-boxed FAI value.
#[derive(Clone, Copy)]
pub struct Value(u64);

impl Value {
    // ── Constructors ───────────────────────────────────────────────

    #[inline(always)]
    pub fn float(f: f64) -> Self {
        Self(f.to_bits())
    }

    #[inline(always)]
    pub fn int(i: i32) -> Self {
        Self(QNAN | TAG_INT | (i as u32 as u64))
    }

    #[inline(always)]
    pub fn bool(b: bool) -> Self {
        Self(QNAN | TAG_BOOL | (b as u64))
    }

    #[inline(always)]
    pub fn null() -> Self {
        Self(QNAN | TAG_NULL)
    }

    /// Reconstruct a `Value` from its raw NaN-boxed 64-bit pattern.
    /// Mirror of `bits()` — used by the wasm host when unboxing values
    /// read from guest memory before handing them to the FFI bridge.
    #[inline(always)]
    pub fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Equivalent to `bits()` but named to mirror Rust's `to_bits` on
    /// `f64`. Kept as an alias so callers reading/writing guest memory
    /// don't need to juggle two names.
    #[inline(always)]
    pub fn to_bits(self) -> u64 {
        self.0
    }

    #[inline(always)]
    pub fn void() -> Self {
        Self(QNAN | TAG_VOID)
    }

    #[inline(always)]
    pub fn object(ptr: GcRef) -> Self {
        let addr = ptr.as_ptr() as u64;
        debug_assert!(
            addr & 0xFFFF_0000_0000_0000 == 0,
            "pointer must fit in 48 bits"
        );
        Self(QNAN | SIGN_BIT | addr)
    }

    // ── Type checks ────────────────────────────────────────────────

    #[inline(always)]
    pub fn is_float(self) -> bool {
        // A value is a float if it's NOT a NaN-boxed tagged value.
        // Either it's a normal f64, or it's the canonical NaN.
        (self.0 & QNAN) != QNAN || self.0 == f64::NAN.to_bits()
    }

    #[inline(always)]
    pub fn is_int(self) -> bool {
        (self.0 & (QNAN | SIGN_BIT | 0x0007_0000_0000_0000)) == (QNAN | TAG_INT)
    }

    #[inline(always)]
    pub fn is_bool(self) -> bool {
        (self.0 & (QNAN | SIGN_BIT | 0x0007_0000_0000_0000)) == (QNAN | TAG_BOOL)
    }

    #[inline(always)]
    pub fn is_null(self) -> bool {
        self.0 == (QNAN | TAG_NULL)
    }

    #[inline(always)]
    pub fn is_void(self) -> bool {
        self.0 == (QNAN | TAG_VOID)
    }

    #[inline(always)]
    pub fn is_object(self) -> bool {
        (self.0 & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT)
    }

    // ── Accessors ──────────────────────────────────────────────────

    #[inline(always)]
    pub fn as_float(self) -> f64 {
        f64::from_bits(self.0)
    }

    #[inline(always)]
    pub fn as_int(self) -> i32 {
        self.0 as u32 as i32
    }

    #[inline(always)]
    pub fn as_bool(self) -> bool {
        (self.0 & 1) != 0
    }

    #[inline(always)]
    pub fn as_object(self) -> GcRef {
        let addr = self.0 & 0x0000_FFFF_FFFF_FFFF;
        unsafe { GcRef::from_ptr(addr as *mut Object) }
    }

    /// Get the raw bits (for equality comparison of tagged values).
    #[inline(always)]
    pub fn bits(self) -> u64 {
        self.0
    }

    // ── Numeric helpers ────────────────────────────────────────────

    /// Returns the numeric value as f64 (works for both Int and Float).
    #[inline(always)]
    pub fn as_number(self) -> f64 {
        if self.is_int() {
            self.as_int() as f64
        } else {
            self.as_float()
        }
    }

    /// True if this value is truthy (only `true` is truthy in FAI — strict bools).
    #[inline(always)]
    pub fn is_truthy(self) -> bool {
        self.is_bool() && self.as_bool()
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        // For floats, use f64 equality (NaN != NaN). For everything else, bitwise.
        if self.is_float() && other.is_float() {
            self.as_float() == other.as_float()
        } else {
            self.0 == other.0
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_null() {
            write!(f, "null")
        } else if self.is_void() {
            write!(f, "void")
        } else if self.is_bool() {
            write!(f, "{}", self.as_bool())
        } else if self.is_int() {
            write!(f, "{}", self.as_int())
        } else if self.is_object() {
            write!(f, "<object@{:p}>", self.as_object().as_ptr())
        } else {
            // Float
            write!(f, "{}", self.as_float())
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_null() {
            write!(f, "null")
        } else if self.is_void() {
            Ok(())
        } else if self.is_bool() {
            write!(f, "{}", self.as_bool())
        } else if self.is_int() {
            write!(f, "{}", self.as_int())
        } else if self.is_float() {
            let v = self.as_float();
            if v == v.floor() && v.is_finite() {
                write!(f, "{:.1}", v)
            } else {
                write!(f, "{}", v)
            }
        } else if self.is_object() {
            // Object display requires heap access; the wasm runner's
            // nan_box helpers handle this at the boundary.
            write!(f, "<object>")
        } else {
            write!(f, "<unknown>")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_int_roundtrip() {
        for i in [0, 1, -1, 42, i32::MAX, i32::MIN] {
            let v = Value::int(i);
            assert!(v.is_int());
            assert!(!v.is_float());
            assert!(!v.is_object());
            assert_eq!(v.as_int(), i);
        }
    }

    #[test]
    fn test_float_roundtrip() {
        for f in [0.0, 1.5, -3.14, f64::INFINITY, f64::NEG_INFINITY] {
            let v = Value::float(f);
            assert!(v.is_float());
            assert!(!v.is_int());
            assert_eq!(v.as_float(), f);
        }
    }

    #[test]
    fn test_bool_roundtrip() {
        assert!(Value::bool(true).is_bool());
        assert!(Value::bool(true).as_bool());
        assert!(!Value::bool(false).as_bool());
    }

    #[test]
    fn test_null_void() {
        assert!(Value::null().is_null());
        assert!(Value::void().is_void());
        assert!(!Value::null().is_void());
        assert!(!Value::void().is_null());
    }

    #[test]
    fn test_equality() {
        assert_eq!(Value::int(42), Value::int(42));
        assert_ne!(Value::int(42), Value::int(43));
        assert_eq!(Value::null(), Value::null());
        assert_eq!(Value::bool(true), Value::bool(true));
        assert_ne!(Value::bool(true), Value::bool(false));
        assert_eq!(Value::float(3.14), Value::float(3.14));
    }

    #[test]
    fn test_as_number_int() {
        let v = Value::int(42);
        assert_eq!(v.as_number(), 42.0);
    }

    #[test]
    fn test_as_number_float() {
        let v = Value::float(3.14);
        assert_eq!(v.as_number(), 3.14);
    }

    #[test]
    fn test_type_checks_mutually_exclusive() {
        let int = Value::int(1);
        assert!(int.is_int());
        assert!(!int.is_float());
        assert!(!int.is_bool());
        assert!(!int.is_null());
        assert!(!int.is_void());
        assert!(!int.is_object());

        let float = Value::float(1.0);
        assert!(float.is_float());
        assert!(!float.is_int());
        assert!(!float.is_bool());
        assert!(!float.is_null());

        let b = Value::bool(true);
        assert!(b.is_bool());
        assert!(!b.is_int());
        assert!(!b.is_float());
        assert!(!b.is_null());
    }

    #[test]
    fn test_is_truthy() {
        assert!(Value::bool(true).is_truthy());
        assert!(!Value::bool(false).is_truthy());
        assert!(!Value::int(1).is_truthy());
        assert!(!Value::null().is_truthy());
    }

    #[test]
    fn test_float_zero() {
        let v = Value::float(0.0);
        assert!(v.is_float());
        assert!(!v.is_int());
        assert_eq!(v.as_float(), 0.0);
    }
}
