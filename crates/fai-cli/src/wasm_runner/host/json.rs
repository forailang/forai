//! JSON host imports: `json_parse`, `json_stringify`.

use wasmtime::*;

use super::super::heap::{build_value, wasm_alloc_str};
use super::super::nan_box::{
    ADDR_MASK, OBJ_TAG_ARRAY, OBJ_TAG_DICT, OBJ_TAG_STRING, QNAN, SIGN_BIT, TAG_BOOL, TAG_INT,
    TAG_MASK, TAG_NULL, TAG_VOID, VAL_NULL,
};

// Object tag for Instance heap objects (not exposed by nan_box module).
const OBJ_TAG_INSTANCE: i32 = 7;

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.json_parse(ptr, len) -> i64 (NaN-boxed value)
    linker
        .func_wrap(
            "env",
            "json_parse",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let json_str = {
                    let data = mem.data(&caller);
                    String::from_utf8_lossy(&data[ptr as usize..(ptr + len) as usize]).into_owned()
                };
                match serde_json::from_str::<serde_json::Value>(&json_str) {
                    Ok(v) => build_value(&mut caller, &mem, &v),
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.json_require_string(dict_val, key_ptr, key_len) -> i64.
    // Walks the guest-heap dict looking for `key`; returns the value if
    // it's a String, else VAL_NULL. VM diverges by raising typed errors
    // (KeyError / TypeError); the wasm path returns null instead.
    linker
        .func_wrap(
            "env",
            "json_require_string",
            |mut caller: Caller<'_, ()>, dict_val: i64, key_ptr: i32, key_len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let key = if (key_ptr + key_len) as usize <= data.len() {
                    String::from_utf8_lossy(&data[key_ptr as usize..(key_ptr + key_len) as usize])
                        .into_owned()
                } else {
                    return VAL_NULL;
                };
                // The dict must be a heap object pointer.
                let dv = dict_val as u64;
                if (dv & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
                    return VAL_NULL;
                }
                let addr = (dv & ADDR_MASK) as usize;
                if addr + 8 > data.len() {
                    return VAL_NULL;
                }
                let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().unwrap());
                let count =
                    i32::from_le_bytes(data[addr + 4..addr + 8].try_into().unwrap()) as usize;
                let entry_base = match tag {
                    t if t == OBJ_TAG_DICT => 8,
                    t if t == OBJ_TAG_INSTANCE => 16,
                    _ => return VAL_NULL,
                };
                for i in 0..count {
                    let ea = addr + entry_base + i * 16;
                    if ea + 16 > data.len() {
                        break;
                    }
                    let k = u64::from_le_bytes(data[ea..ea + 8].try_into().unwrap());
                    let v = u64::from_le_bytes(data[ea + 8..ea + 16].try_into().unwrap());
                    if (k & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
                        continue;
                    }
                    let kaddr = (k & ADDR_MASK) as usize;
                    if kaddr + 8 > data.len() {
                        continue;
                    }
                    let klen =
                        i32::from_le_bytes(data[kaddr + 4..kaddr + 8].try_into().unwrap()) as usize;
                    let kstart = kaddr + 8;
                    let kend = kstart.saturating_add(klen);
                    if kend > data.len() {
                        continue;
                    }
                    let k_str = std::str::from_utf8(&data[kstart..kend]).unwrap_or("");
                    if k_str == key {
                        // Verify value is a String object.
                        if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
                            return VAL_NULL;
                        }
                        let vaddr = (v & ADDR_MASK) as usize;
                        if vaddr + 8 > data.len() {
                            return VAL_NULL;
                        }
                        let vtag = i32::from_le_bytes(data[vaddr..vaddr + 4].try_into().unwrap());
                        if vtag == OBJ_TAG_STRING {
                            return v as i64;
                        }
                        return VAL_NULL;
                    }
                }
                VAL_NULL
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.json_stringify(val) -> i64 (NaN-boxed String).
    //
    // Walks the guest-heap value and emits JSON bytes, then allocates
    // a String object on the guest heap. Supports String, Int, Float,
    // Bool, Null, Array, Dict, and Instance (rendered as object).
    // Mirrors `native_json_stringify` in fai-runtime/src/natives.rs.
    linker
        .func_wrap(
            "env",
            "json_stringify",
            |mut caller: Caller<'_, ()>, val: i64| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let mut out = String::new();
                {
                    let data = mem.data(&caller);
                    stringify_value(data, val as u64, &mut out);
                }
                wasm_alloc_str(&mut caller, &mem, &out)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.json_query(json_ptr, json_len, path_ptr, path_len) -> i64.
    //
    // Host-side JSON selection: parse with serde, evaluate the dot-path
    // (`a.b[].c` — `seg[]` expands arrays, empty path selects the root),
    // and materialize ONLY the matched values as a guest Array. The full
    // document never becomes a guest tree, so multi-MB payloads cost
    // guest allocation proportional to the matches, not the document.
    // Returns VAL_NULL on invalid JSON (same convention as json_parse).
    // The browser runtime implements a JS twin with identical semantics.
    linker
        .func_wrap(
            "env",
            "json_query",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32, p_ptr: i32, p_len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let (text, path) = {
                    let data = mem.data(&caller);
                    (
                        read_guest_str(data, ptr, len),
                        read_guest_str(data, p_ptr, p_len),
                    )
                };
                let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
                    return VAL_NULL;
                };
                let matches = eval_json_path(&root, &path);
                let arr =
                    serde_json::Value::Array(matches.into_iter().cloned().collect());
                build_value(&mut caller, &mem, &arr)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.json_query_page(json_ptr, json_len, path_ptr, path_len,
    //                     offset, limit) -> i64.
    //
    // Windowed variant of json_query: returns a guest Dict
    // `{ total: Int, items: Array }` where `items` is `matches[offset ..
    // offset+limit]`. Offset clamps into range; a non-positive limit
    // yields an empty window (the total still reports). VAL_NULL on
    // invalid JSON.
    linker
        .func_wrap(
            "env",
            "json_query_page",
            |mut caller: Caller<'_, ()>,
             ptr: i32,
             len: i32,
             p_ptr: i32,
             p_len: i32,
             offset: i32,
             limit: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let (text, path) = {
                    let data = mem.data(&caller);
                    (
                        read_guest_str(data, ptr, len),
                        read_guest_str(data, p_ptr, p_len),
                    )
                };
                let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
                    return VAL_NULL;
                };
                let matches = eval_json_path(&root, &path);
                let total = matches.len();
                let start = (offset.max(0) as usize).min(total);
                let take = limit.max(0) as usize;
                let items: Vec<serde_json::Value> = matches[start..]
                    .iter()
                    .take(take)
                    .map(|v| (*v).clone())
                    .collect();
                let page = serde_json::json!({
                    "total": total,
                    "items": items,
                });
                build_value(&mut caller, &mem, &page)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

/// Copy a guest string range out of linear memory (lossy on bad UTF-8,
/// empty on an out-of-bounds range).
fn read_guest_str(data: &[u8], ptr: i32, len: i32) -> String {
    let (ptr, len) = (ptr as usize, len as usize);
    match data.get(ptr..ptr + len) {
        Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        None => String::new(),
    }
}

/// Evaluate a bounded jq-like dot-path against a parsed document and
/// return references to every match, in document order.
///
/// Dialect: segments are separated by `.`; a segment `name` selects that
/// object field (objects lacking the field drop out silently); a trailing
/// `[]` on a segment expands the selected array into its elements
/// (non-arrays drop out); a bare `[]` segment expands the current values;
/// an empty path (or only empty segments) selects the root. A leading
/// `.` (jq style, `.a.b`) is tolerated via the empty-segment skip.
fn eval_json_path<'v>(root: &'v serde_json::Value, path: &str) -> Vec<&'v serde_json::Value> {
    let mut current: Vec<&'v serde_json::Value> = vec![root];
    for raw in path.split('.') {
        let segment = raw.trim();
        if segment.is_empty() {
            continue;
        }
        let expands = segment.ends_with("[]");
        let field = if expands {
            &segment[..segment.len() - 2]
        } else {
            segment
        };
        let mut next: Vec<&'v serde_json::Value> = Vec::new();
        for value in current {
            let selected = if field.is_empty() {
                Some(value)
            } else {
                value.get(field)
            };
            let Some(selected) = selected else { continue };
            if expands {
                if let serde_json::Value::Array(items) = selected {
                    next.extend(items.iter());
                }
            } else {
                next.push(selected);
            }
        }
        current = next;
    }
    current
}

#[cfg(test)]
mod eval_json_path_tests {
    use super::eval_json_path;
    use serde_json::json;

    #[test]
    fn empty_path_selects_root() {
        let doc = json!({"a": 1});
        let m = eval_json_path(&doc, "");
        assert_eq!(m, vec![&doc]);
    }

    #[test]
    fn field_chain_selects_nested_value() {
        let doc = json!({"a": {"b": {"c": 42}}});
        let m = eval_json_path(&doc, "a.b.c");
        assert_eq!(m, vec![&json!(42)]);
    }

    #[test]
    fn leading_dot_is_tolerated() {
        let doc = json!({"a": 1});
        assert_eq!(eval_json_path(&doc, ".a"), vec![&json!(1)]);
    }

    #[test]
    fn expansion_fans_out_and_selects_each() {
        let doc = json!({"items": [{"s": "x"}, {"s": "y"}, {"n": 3}]});
        let m = eval_json_path(&doc, "items[].s");
        assert_eq!(m, vec![&json!("x"), &json!("y")]);
    }

    #[test]
    fn bare_expansion_expands_root_array() {
        let doc = json!([1, 2, 3]);
        let m = eval_json_path(&doc, "[]");
        assert_eq!(m, vec![&json!(1), &json!(2), &json!(3)]);
    }

    #[test]
    fn missing_field_and_non_array_expansion_drop_out() {
        let doc = json!({"a": {"b": 1}, "s": "text"});
        assert!(eval_json_path(&doc, "a.missing").is_empty());
        assert!(eval_json_path(&doc, "s[]").is_empty());
    }
}

/// Serialize a NaN-boxed guest value into `out`.
///
/// `data` is a snapshot of the guest's linear memory — the caller must
/// ensure no intervening mutation. Unknown tags degrade to `"null"` so
/// bad input can't panic the host.
fn stringify_value(data: &[u8], val: u64, out: &mut String) {
    // VOID / NULL → "null".
    if val == (QNAN | TAG_VOID) || val == (QNAN | TAG_NULL) {
        out.push_str("null");
        return;
    }
    // Int.
    if (val & (QNAN | SIGN_BIT | TAG_MASK)) == (QNAN | TAG_INT) {
        let i = val as i32;
        out.push_str(&i.to_string());
        return;
    }
    // Bool.
    if val == (QNAN | TAG_BOOL) {
        out.push_str("false");
        return;
    }
    if val == (QNAN | TAG_BOOL | 1) {
        out.push_str("true");
        return;
    }
    // Object pointer.
    if (val & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT) {
        let addr = (val & ADDR_MASK) as usize;
        stringify_object(data, addr, out);
        return;
    }
    // Float (non-NaN-boxed bits = raw f64).
    if (val & QNAN) != QNAN {
        let f = f64::from_bits(val);
        // Match VM behaviour: whole-valued finite floats render without
        // a decimal, everything else uses default {} formatting.
        if f.is_finite() && f == f.floor() {
            out.push_str(&format!("{}", f as i64));
        } else {
            out.push_str(&format!("{}", f));
        }
        return;
    }
    out.push_str("null");
}

fn stringify_object(data: &[u8], addr: usize, out: &mut String) {
    if addr + 8 > data.len() {
        out.push_str("null");
        return;
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().unwrap());
    let count = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().unwrap()) as usize;
    match tag {
        t if t == OBJ_TAG_STRING => {
            let end = addr.saturating_add(8).saturating_add(count);
            let bytes = if end <= data.len() {
                &data[addr + 8..end]
            } else {
                &[][..]
            };
            let s = String::from_utf8_lossy(bytes);
            write_json_string(&s, out);
        }
        t if t == OBJ_TAG_ARRAY => {
            out.push('[');
            for i in 0..count {
                if i > 0 {
                    out.push(',');
                }
                let off = addr + 8 + i * 8;
                if off + 8 > data.len() {
                    out.push_str("null");
                    continue;
                }
                let v = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
                stringify_value(data, v, out);
            }
            out.push(']');
        }
        t if t == OBJ_TAG_DICT || t == OBJ_TAG_INSTANCE => {
            // Instance: key entries start at offset 16 (type_name occupies
            // offset 8). Dict: entries start at offset 8.
            let entry_base = if t == OBJ_TAG_INSTANCE { 16 } else { 8 };
            out.push('{');
            for i in 0..count {
                if i > 0 {
                    out.push(',');
                }
                let ea = addr + entry_base + i * 16;
                if ea + 16 > data.len() {
                    continue;
                }
                let k = u64::from_le_bytes(data[ea..ea + 8].try_into().unwrap());
                let v = u64::from_le_bytes(data[ea + 8..ea + 16].try_into().unwrap());
                // Key: always an object-ref string.
                if (k & (QNAN | SIGN_BIT)) == (QNAN | SIGN_BIT) {
                    let kaddr = (k & ADDR_MASK) as usize;
                    if kaddr + 8 <= data.len() {
                        let klen =
                            i32::from_le_bytes(data[kaddr + 4..kaddr + 8].try_into().unwrap())
                                as usize;
                        let start = kaddr + 8;
                        let end = start.saturating_add(klen);
                        if end <= data.len() {
                            let s = String::from_utf8_lossy(&data[start..end]);
                            write_json_string(&s, out);
                        } else {
                            out.push_str("\"\"");
                        }
                    } else {
                        out.push_str("\"\"");
                    }
                } else {
                    out.push_str("\"\"");
                }
                out.push(':');
                stringify_value(data, v, out);
            }
            out.push('}');
        }
        _ => {
            out.push_str("null");
        }
    }
}

/// Write a JSON-escaped string literal (with surrounding quotes).
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
