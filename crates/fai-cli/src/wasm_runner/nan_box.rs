//! NaN-boxed value encoding/decoding.
//!
//! FAI runtime values are encoded as 64-bit patterns where non-NaN doubles
//! represent floats and NaN bits with tag bits encode everything else.
//! This module owns the constants and pure classification helpers so they
//! can be unit-tested without spinning up a WASM instance.

pub(crate) const QNAN: u64 = 0x7FFC_0000_0000_0000;
pub(crate) const SIGN_BIT: u64 = 0x8000_0000_0000_0000;

pub(crate) const TAG_NULL: u64 = 0x0001_0000_0000_0000;
pub(crate) const TAG_VOID: u64 = 0x0002_0000_0000_0000;
pub(crate) const TAG_BOOL: u64 = 0x0003_0000_0000_0000;
pub(crate) const TAG_INT: u64 = 0x0004_0000_0000_0000;

/// Mask covering the three tag bits in the qNaN payload.
pub(crate) const TAG_MASK: u64 = 0x0007_0000_0000_0000;

/// Mask covering the low 48 bits used as an object pointer.
pub(crate) const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Canonical null value (NaN-boxed).
pub(crate) const VAL_NULL: i64 = (QNAN | TAG_NULL) as i64;

/// Canonical void value (NaN-boxed).
pub(crate) const VAL_VOID: i64 = (QNAN | TAG_VOID) as i64;

// Object-header tags (written as the first i32 of a heap-allocated object).
pub(crate) const OBJ_TAG_STRING: i32 = 0;
pub(crate) const OBJ_TAG_ARRAY: i32 = 1;
pub(crate) const OBJ_TAG_TUPLE: i32 = 2;
pub(crate) const OBJ_TAG_DICT: i32 = 3;
pub(crate) const OBJ_TAG_CLOSURE: i32 = 4;

/// Classified result of a top-level `_start` return.
#[derive(Debug, PartialEq)]
pub(super) enum ReturnKind {
    Void,
    Int(i32),
    Bool(bool),
    Null,
    Object(usize),
    Float(f64),
    Unknown,
}

/// Classify a raw NaN-boxed value for printing. Pure.
pub(super) fn classify_return_value(val: u64) -> ReturnKind {
    if val == VAL_VOID as u64 {
        return ReturnKind::Void;
    }
    if (val & (QNAN | SIGN_BIT | TAG_MASK)) == (QNAN | TAG_INT) {
        return ReturnKind::Int(val as i32);
    }
    if val == (QNAN | TAG_BOOL | 1) {
        return ReturnKind::Bool(true);
    }
    if val == (QNAN | TAG_BOOL) {
        return ReturnKind::Bool(false);
    }
    if val == (QNAN | TAG_NULL) {
        return ReturnKind::Null;
    }
    if (val & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT) {
        return ReturnKind::Object((val & ADDR_MASK) as usize);
    }
    if (val & QNAN) != QNAN {
        return ReturnKind::Float(f64::from_bits(val));
    }
    ReturnKind::Unknown
}

/// Format a float the way FAI expects: whole-valued finite floats render
/// without a decimal (`3` not `3.0`), everything else uses default `{}`.
pub(super) fn format_float(val: f64) -> String {
    if val.is_finite() && val == val.floor() {
        format!("{}", val as i64)
    } else {
        format!("{}", val)
    }
}

/// Encode an object pointer as a NaN-boxed value.
pub(super) fn encode_object(addr: u32) -> i64 {
    (QNAN | SIGN_BIT | addr as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_int(i: i32) -> u64 {
        QNAN | TAG_INT | (i as u32 as u64)
    }

    fn encode_bool(b: bool) -> u64 {
        QNAN | TAG_BOOL | if b { 1 } else { 0 }
    }

    #[test]
    fn test_classify_void() {
        assert_eq!(classify_return_value(VAL_VOID as u64), ReturnKind::Void);
    }

    #[test]
    fn test_classify_null() {
        assert_eq!(classify_return_value(VAL_NULL as u64), ReturnKind::Null);
    }

    #[test]
    fn test_classify_int_positive() {
        assert_eq!(classify_return_value(encode_int(42)), ReturnKind::Int(42));
    }

    #[test]
    fn test_classify_int_negative() {
        assert_eq!(classify_return_value(encode_int(-7)), ReturnKind::Int(-7));
    }

    #[test]
    fn test_classify_int_zero() {
        assert_eq!(classify_return_value(encode_int(0)), ReturnKind::Int(0));
    }

    #[test]
    fn test_classify_bool_true() {
        assert_eq!(
            classify_return_value(encode_bool(true)),
            ReturnKind::Bool(true)
        );
    }

    #[test]
    fn test_classify_bool_false() {
        assert_eq!(
            classify_return_value(encode_bool(false)),
            ReturnKind::Bool(false)
        );
    }

    #[test]
    fn test_classify_object_extracts_addr() {
        let addr: u32 = 0x1234;
        let val = encode_object(addr) as u64;
        assert_eq!(
            classify_return_value(val),
            ReturnKind::Object(addr as usize)
        );
    }

    #[test]
    fn test_classify_object_large_addr() {
        let addr: u32 = 0xFFFF_FFFE;
        let val = encode_object(addr) as u64;
        assert_eq!(
            classify_return_value(val),
            ReturnKind::Object(addr as usize)
        );
    }

    #[test]
    fn test_classify_float_integer_valued() {
        let bits = 1.0_f64.to_bits();
        match classify_return_value(bits) {
            ReturnKind::Float(f) => assert_eq!(f, 1.0),
            other => panic!("expected Float(1.0), got {:?}", other),
        }
    }

    #[test]
    fn test_classify_float_fractional() {
        let bits = 3.14_f64.to_bits();
        match classify_return_value(bits) {
            ReturnKind::Float(f) => assert!((f - 3.14).abs() < 1e-10),
            other => panic!("expected Float(3.14), got {:?}", other),
        }
    }

    #[test]
    fn test_classify_float_negative() {
        let bits = (-2.5_f64).to_bits();
        match classify_return_value(bits) {
            ReturnKind::Float(f) => assert_eq!(f, -2.5),
            other => panic!("expected Float(-2.5), got {:?}", other),
        }
    }

    #[test]
    fn test_format_float_integer_valued() {
        assert_eq!(format_float(3.0), "3");
        assert_eq!(format_float(-7.0), "-7");
        assert_eq!(format_float(0.0), "0");
    }

    #[test]
    fn test_format_float_fractional() {
        assert_eq!(format_float(3.14), "3.14");
        assert_eq!(format_float(-0.5), "-0.5");
    }

    #[test]
    fn test_format_float_non_finite_falls_through() {
        // Non-finite values use default formatting (no i64 cast)
        assert_eq!(format_float(f64::INFINITY), format!("{}", f64::INFINITY));
        assert_eq!(
            format_float(f64::NEG_INFINITY),
            format!("{}", f64::NEG_INFINITY)
        );
        let nan_s = format_float(f64::NAN);
        assert_eq!(nan_s, format!("{}", f64::NAN));
    }

    #[test]
    fn test_encode_object_sets_qnan_and_sign() {
        let v = encode_object(0xAA) as u64;
        assert_eq!(v & QNAN, QNAN);
        assert_eq!(v & SIGN_BIT, SIGN_BIT);
        assert_eq!(v & ADDR_MASK, 0xAA);
    }
}
