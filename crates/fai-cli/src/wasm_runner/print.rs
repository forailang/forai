//! Decode and print the top-level `_start` return value.

use wasmtime::*;

use super::nan_box::{classify_return_value, format_float, ReturnKind, OBJ_TAG_STRING};
use super::output;

/// If the return value is a printable FAI value, print it to the host stdout
/// sink (real stdout by default; a capture buffer when a `CaptureGuard` is
/// active). Void values produce no output.
pub(super) fn print_return_value(
    result: i64,
    instance: &Instance,
    mut store: impl AsContextMut<Data = ()>,
) {
    let val = result as u64;
    match classify_return_value(val) {
        ReturnKind::Void => {}
        ReturnKind::Int(i) => output::stdout_line(&i.to_string()),
        ReturnKind::Bool(true) => output::stdout_line("true"),
        ReturnKind::Bool(false) => output::stdout_line("false"),
        ReturnKind::Null => output::stdout_line("null"),
        ReturnKind::Object(addr) => {
            // Only strings are printed; other objects are ignored.
            if let Some(mem) = instance.get_memory(&mut store, "memory") {
                let data = mem.data(&store);
                if addr + 8 <= data.len() {
                    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().unwrap());
                    if tag == OBJ_TAG_STRING {
                        let len = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().unwrap())
                            as usize;
                        if addr + 8 + len <= data.len() {
                            let s = std::str::from_utf8(&data[addr + 8..addr + 8 + len])
                                .unwrap_or("<invalid utf8>");
                            output::stdout_line(s);
                        }
                    }
                }
            }
        }
        ReturnKind::Float(f) => output::stdout_line(&format_float(f)),
        ReturnKind::Unknown => {}
    }
}
