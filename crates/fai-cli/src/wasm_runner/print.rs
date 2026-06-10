//! Decode and print the top-level `_start` return value.

use wasmtime::*;

use super::nan_box::{
    classify_return_value, format_float, ReturnKind, OBJ_TAG_DICT, OBJ_TAG_STRING,
};
use super::output;

/// If the return value is a printable FAI value, print it to the host stdout
/// sink (real stdout by default; a capture buffer when a `CaptureGuard` is
/// active). Void values produce no output.
pub(super) fn print_return_value(
    result: i64,
    instance: &Instance,
    mut store: impl AsContextMut<Data = ()>,
) {
    if let Some(text) = format_return_value(result, instance, &mut store) {
        output::stdout_line(&text);
    }
}

pub(super) fn format_return_value(
    result: i64,
    instance: &Instance,
    mut store: impl AsContextMut<Data = ()>,
) -> Option<String> {
    match classify_return_value(result as u64) {
        ReturnKind::Void => {}
        ReturnKind::Int(i) => return Some(i.to_string()),
        ReturnKind::Bool(true) => return Some("true".to_string()),
        ReturnKind::Bool(false) => return Some("false".to_string()),
        ReturnKind::Null => return Some("null".to_string()),
        ReturnKind::Object(addr) => {
            // Strings print directly; an `Error` dict prints its `message`
            // field (so a failed async task reports a useful message rather
            // than `<unprintable>`). Other objects are ignored.
            if let Some(mem) = instance.get_memory(&mut store, "memory") {
                let data = mem.data(&store);
                let read_i32 = |off: usize| -> Option<i32> {
                    data.get(off..off + 4)
                        .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
                };
                let read_i64 = |off: usize| -> Option<i64> {
                    data.get(off..off + 8)
                        .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
                };
                // Decode a NaN-boxed string object at `obj` into text.
                let decode_string = |obj: i64| -> Option<String> {
                    let ReturnKind::Object(saddr) = classify_return_value(obj as u64) else {
                        return None;
                    };
                    if read_i32(saddr) != Some(OBJ_TAG_STRING) {
                        return None;
                    }
                    let len = read_i32(saddr + 4).unwrap_or(0) as usize;
                    let bytes = data.get(saddr + 8..saddr + 8 + len)?;
                    Some(std::str::from_utf8(bytes).unwrap_or("<invalid utf8>").to_string())
                };
                match read_i32(addr) {
                    Some(OBJ_TAG_STRING) => {
                        if let Some(s) = decode_string(result) {
                            return Some(s);
                        }
                    }
                    Some(OBJ_TAG_DICT) => {
                        // Walk entries (key i64, value i64; 16 bytes each from
                        // offset 8) for a "message" string key and print it.
                        let count = read_i32(addr + 4).unwrap_or(0).max(0) as usize;
                        for i in 0..count {
                            let entry = addr + 8 + i * 16;
                            let Some(key) = read_i64(entry) else { break };
                            let is_message = matches!(classify_return_value(key as u64),
                                ReturnKind::Object(kaddr)
                                    if read_i32(kaddr) == Some(OBJ_TAG_STRING)
                                        && data.get(
                                            kaddr + 8
                                                ..kaddr + 8 + read_i32(kaddr + 4).unwrap_or(0) as usize,
                                        ) == Some(b"message".as_slice()));
                            if is_message {
                                if let Some(v) = read_i64(entry + 8) {
                                    if let Some(s) = decode_string(v) {
                                        return Some(s);
                                    }
                                }
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        ReturnKind::Float(f) => return Some(format_float(f)),
        ReturnKind::Unknown => {}
    }
    None
}
