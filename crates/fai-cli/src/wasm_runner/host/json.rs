//! JSON host imports: `json_parse`, `json_stringify`.

use wasmtime::*;

use super::host_ops::signal_host_error;
use super::super::heap::{build_value, wasm_alloc_str};
use super::super::nan_box::{
    ADDR_MASK, OBJ_TAG_ARRAY, OBJ_TAG_DICT, OBJ_TAG_SECRET, OBJ_TAG_STRING, QNAN, SIGN_BIT,
    TAG_BOOL, TAG_INT,
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
                    // Malformed JSON raises a catchable forai error (via the
                    // __error_flag / __error_value channel) instead of silently
                    // returning null — codegen marks IMPORT_JSON_PARSE as
                    // error-signaling so the nearest try/catch (or the caller)
                    // sees it. The serde message names the offending
                    // line:column.
                    Err(e) => signal_host_error(
                        &mut caller,
                        "json",
                        &format!("json.parse: invalid JSON: {}", e),
                    ),
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
    // Host-side JSON selection: parse with serde, evaluate a jq-style
    // selection path (see `parse_json_path` for the grammar), and
    // materialize ONLY the matched values as a guest Array. The full
    // document never becomes a guest tree, so multi-MB payloads cost
    // guest allocation proportional to the matches, not the document.
    // Returns VAL_NULL on invalid JSON (same convention as json_parse)
    // and on malformed paths. The browser runtime's faiJsonQueryEval is
    // the JS twin — keep the two grammars identical.
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
                let Ok(matches) = eval_query(&root, &path) else {
                    return VAL_NULL; // unparseable query, same convention as bad JSON
                };
                let arr = serde_json::Value::Array(matches);
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
                // A query that parses in neither engine returns a page carrying
                // an `error` field (not VAL_NULL) so the caller can distinguish
                // "bad query" from "valid query, zero matches" and surface a
                // loud message instead of silently retrying variants.
                let matches = match eval_query(&root, &path) {
                    Ok(matches) => matches,
                    Err(msg) => {
                        let page = serde_json::json!({
                            "total": 0,
                            "items": [],
                            "error": msg,
                        });
                        return build_value(&mut caller, &mem, &page);
                    }
                };
                let total = matches.len();
                let start = (offset.max(0) as usize).min(total);
                let take = limit.max(0) as usize;
                let items: Vec<serde_json::Value> =
                    matches.into_iter().skip(start).take(take).collect();
                let page = serde_json::json!({
                    "total": total,
                    "items": items,
                });
                build_value(&mut caller, &mem, &page)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.json_format(ptr, len) -> i64 — pretty-print a JSON string with
    // 2-space indent (serde's pretty writer), one attribute per line.
    // VAL_NULL on invalid JSON. Native normalizes object key order; the
    // browser twin preserves insertion order.
    linker
        .func_wrap(
            "env",
            "json_format",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let text = {
                    let data = mem.data(&caller);
                    read_guest_str(data, ptr, len)
                };
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(v) => {
                        let pretty = serde_json::to_string_pretty(&v)
                            .unwrap_or_else(|_| v.to_string());
                        wasm_alloc_str(&mut caller, &mem, &pretty)
                    }
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.json_minify(ptr, len) -> i64 — reserialize compactly. VAL_NULL
    // on invalid JSON.
    linker
        .func_wrap(
            "env",
            "json_minify",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let text = {
                    let data = mem.data(&caller);
                    read_guest_str(data, ptr, len)
                };
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(v) => wasm_alloc_str(&mut caller, &mem, &v.to_string()),
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.json_valid(ptr, len) -> i32 — 1 when the string parses as
    // JSON. Nothing is materialized either host- or guest-side beyond
    // the serde parse itself.
    linker
        .func_wrap(
            "env",
            "json_valid",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let text = read_guest_str(data, ptr, len);
                serde_json::from_str::<serde::de::IgnoredAny>(&text).is_ok() as i32
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.json_stringify_pretty(val) -> i64 — json_stringify with 2-space
    // pretty output: walk the guest value into compact JSON (the existing
    // stringify_value walker), then reserialize pretty via serde. Falls
    // back to the compact form if the round-trip ever fails.
    linker
        .func_wrap(
            "env",
            "json_stringify_pretty",
            |mut caller: Caller<'_, ()>, val: i64| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let mut compact = String::new();
                {
                    let data = mem.data(&caller);
                    stringify_value(data, val as u64, &mut compact);
                }
                let pretty = serde_json::from_str::<serde_json::Value>(&compact)
                    .ok()
                    .and_then(|v| serde_json::to_string_pretty(&v).ok())
                    .unwrap_or(compact);
                wasm_alloc_str(&mut caller, &mem, &pretty)
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

/// Evaluate `query` against `root` into the flat match list that the
/// `json_query` / `json_query_page` pagination model expects.
///
/// Dual-dialect, JMESPath-first: JMESPath is a strict superset in power
/// (multi-select `[].{a: x}`, filters `[?flag]`, functions `length(@)`)
/// but NOT in syntax — it has no recursive descent, no `|` pipe spelled
/// jq-style, and rejects a leading `.`. So we try JMESPath first, and a
/// JMESPath *compile* error falls through to the legacy jq-selection
/// engine (`parse_json_path`/`eval_json_path`). Because every jq signature
/// (leading `.`, `..`, `|`, trailing `?`) is a JMESPath syntax error, the
/// existing selection paths route deterministically to the legacy engine
/// with byte-identical behavior, while bare expressions gain JMESPath.
///
/// `Err` only when the query parses in *neither* engine — a genuine syntax
/// error the caller should surface, distinct from a valid query that
/// matched nothing.
fn eval_query(root: &serde_json::Value, query: &str) -> Result<Vec<serde_json::Value>, String> {
    match jmespath::compile(query) {
        Ok(expr) => {
            let found = expr
                .search(root)
                .map_err(|e| format!("jmespath evaluation failed: {e}"))?;
            let value = serde_json::to_value(&*found)
                .map_err(|e| format!("jmespath result not representable as JSON: {e}"))?;
            Ok(flatten_query_result(value))
        }
        Err(jmespath_err) => match eval_json_path(root, query) {
            Some(matches) => Ok(matches.into_iter().cloned().collect()),
            None => {
                // Collapse the JMESPath parser's multi-line caret diagram to a
                // single line: the message flows through a raw-concatenated RPC
                // error envelope, where an embedded newline would produce
                // malformed JSON the client can't parse.
                let reason = jmespath_err
                    .to_string()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                Err(format!(
                    "query is neither valid JMESPath ({reason}) nor a valid \
                     jq-style selection path"
                ))
            }
        },
    }
}

/// Map a single JMESPath result value into the flat match list: `null`
/// (JMESPath's "no match") → no matches; an array → its elements, so a
/// projection like `[].name` pages element-by-element like the legacy
/// engine's fan-out; any other value → a single match.
fn flatten_query_result(value: serde_json::Value) -> Vec<serde_json::Value> {
    match value {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(items) => items,
        other => vec![other],
    }
}

/// One step of a parsed query path. The dialect is the pure *selection*
/// subset of jq — no filters, functions, or object construction.
#[derive(Debug, PartialEq)]
enum PathStep {
    /// `.name`, `."quoted name"`, or `["quoted name"]` — object field.
    Field(String),
    /// `[]` — every element of an array, or every value of an object.
    IterateAll,
    /// `[n]` — one array element; negative indexes from the end.
    Index(i64),
    /// `..` — the value itself plus every descendant, pre-order.
    Descend,
}

/// Parse a jq-style selection path into steps. `None` means the path is
/// malformed (unclosed quote/bracket, slice syntax, stray character) —
/// callers surface that as a null result, mirroring invalid JSON.
///
/// Grammar: steps separated by `.` or `|` (a leading `.` is optional and
/// `|` composes like jq's pipe); bare or double-quoted field names
/// (`.a`, `."has.dots"`); bracket forms `[]`, `[n]`, `[-n]`, `["key"]`;
/// `..` for recursive descent; a `?` suffix is accepted and ignored
/// (selection is already lenient — non-matching values drop out).
fn parse_json_path(path: &str) -> Option<Vec<PathStep>> {
    let b: Vec<char> = path.chars().collect();
    let n = b.len();
    let mut i = 0usize;
    let mut steps: Vec<PathStep> = Vec::new();

    // Read a double-quoted name starting at `b[at] == '"'`; returns the
    // unescaped text and the index just past the closing quote.
    let quoted = |at: usize| -> Option<(String, usize)> {
        let mut j = at + 1;
        let mut out = String::new();
        while j < n {
            match b[j] {
                '"' => return Some((out, j + 1)),
                '\\' if j + 1 < n => {
                    out.push(b[j + 1]);
                    j += 2;
                }
                c => {
                    out.push(c);
                    j += 1;
                }
            }
        }
        None // unclosed quote
    };

    while i < n {
        let c = b[i];
        if c.is_whitespace() || c == '|' || c == '?' {
            i += 1;
            continue;
        }
        if c == '.' {
            if i + 1 < n && b[i + 1] == '.' {
                steps.push(PathStep::Descend);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if c == '"' {
            let (name, next) = quoted(i)?;
            steps.push(PathStep::Field(name));
            i = next;
            continue;
        }
        if c == '[' {
            i += 1;
            while i < n && b[i].is_whitespace() {
                i += 1;
            }
            if i < n && b[i] == ']' {
                steps.push(PathStep::IterateAll);
                i += 1;
                continue;
            }
            if i < n && b[i] == '"' {
                let (name, next) = quoted(i)?;
                i = next;
                while i < n && b[i].is_whitespace() {
                    i += 1;
                }
                if i >= n || b[i] != ']' {
                    return None;
                }
                steps.push(PathStep::Field(name));
                i += 1;
                continue;
            }
            let start = i;
            if i < n && b[i] == '-' {
                i += 1;
            }
            while i < n && b[i].is_ascii_digit() {
                i += 1;
            }
            let digits: String = b[start..i].iter().collect();
            let idx: i64 = digits.parse().ok()?; // "", "-", "1:2" → malformed
            while i < n && b[i].is_whitespace() {
                i += 1;
            }
            if i >= n || b[i] != ']' {
                return None; // unclosed bracket or slice/filter syntax
            }
            steps.push(PathStep::Index(idx));
            i += 1;
            continue;
        }
        if c == ']' {
            return None; // stray close bracket
        }
        // Bare field name: up to the next structural character.
        let start = i;
        while i < n
            && !matches!(b[i], '.' | '|' | '[' | ']' | '?' | '"')
            && !b[i].is_whitespace()
        {
            i += 1;
        }
        if i == start {
            return None;
        }
        steps.push(PathStep::Field(b[start..i].iter().collect()));
    }
    Some(steps)
}

/// Evaluate a jq-style selection path against a parsed document and
/// return references to every match, in document order. `None` means the
/// path itself is malformed.
///
/// Selection is lenient like jq's `?` everywhere: a field on a non-object
/// or a missing key, an index/iterate on the wrong shape — the value
/// drops out silently instead of erroring. An empty path selects the
/// root. `..` enumerates a value and all of its descendants pre-order
/// (arrays by position; objects by serde_json's key order).
fn eval_json_path<'v>(
    root: &'v serde_json::Value,
    path: &str,
) -> Option<Vec<&'v serde_json::Value>> {
    use serde_json::Value;
    let steps = parse_json_path(path)?;
    let mut current: Vec<&'v Value> = vec![root];
    for step in &steps {
        let mut next: Vec<&'v Value> = Vec::new();
        match step {
            PathStep::Field(name) => {
                for value in current {
                    if let Some(selected) = value.get(name.as_str()) {
                        next.push(selected);
                    }
                }
            }
            PathStep::IterateAll => {
                for value in current {
                    match value {
                        Value::Array(items) => next.extend(items.iter()),
                        Value::Object(map) => next.extend(map.values()),
                        _ => {}
                    }
                }
            }
            PathStep::Index(idx) => {
                for value in current {
                    if let Value::Array(items) = value {
                        let k = if *idx < 0 {
                            items.len() as i64 + idx
                        } else {
                            *idx
                        };
                        if k >= 0 && (k as usize) < items.len() {
                            next.push(&items[k as usize]);
                        }
                    }
                }
            }
            PathStep::Descend => {
                for value in current {
                    let mut stack = vec![value];
                    while let Some(v) = stack.pop() {
                        next.push(v);
                        match v {
                            Value::Array(items) => stack.extend(items.iter().rev()),
                            Value::Object(map) => stack.extend(map.values().rev()),
                            _ => {}
                        }
                    }
                }
            }
        }
        current = next;
    }
    Some(current)
}

#[cfg(test)]
mod eval_json_path_tests {
    use super::eval_json_path;
    use serde_json::json;

    fn eval<'v>(
        doc: &'v serde_json::Value,
        path: &str,
    ) -> Vec<&'v serde_json::Value> {
        eval_json_path(doc, path).expect("path should parse")
    }

    #[test]
    fn empty_path_selects_root() {
        let doc = json!({"a": 1});
        assert_eq!(eval(&doc, ""), vec![&doc]);
        assert_eq!(eval(&doc, "."), vec![&doc]);
    }

    #[test]
    fn field_chain_selects_nested_value() {
        let doc = json!({"a": {"b": {"c": 42}}});
        assert_eq!(eval(&doc, "a.b.c"), vec![&json!(42)]);
        assert_eq!(eval(&doc, ".a.b.c"), vec![&json!(42)]);
    }

    #[test]
    fn expansion_fans_out_and_selects_each() {
        let doc = json!({"items": [{"s": "x"}, {"s": "y"}, {"n": 3}]});
        assert_eq!(eval(&doc, "items[].s"), vec![&json!("x"), &json!("y")]);
        assert_eq!(eval(&doc, ".items[].s"), vec![&json!("x"), &json!("y")]);
    }

    #[test]
    fn bare_expansion_expands_root_array() {
        let doc = json!([1, 2, 3]);
        assert_eq!(eval(&doc, "[]"), vec![&json!(1), &json!(2), &json!(3)]);
        assert_eq!(eval(&doc, ".[]"), vec![&json!(1), &json!(2), &json!(3)]);
    }

    #[test]
    fn missing_field_and_non_array_expansion_drop_out() {
        let doc = json!({"a": {"b": 1}, "s": "text"});
        assert!(eval(&doc, "a.missing").is_empty());
        assert!(eval(&doc, "s[]").is_empty());
        assert!(eval(&doc, "a[3]").is_empty());
    }

    #[test]
    fn iterate_all_yields_object_values() {
        let doc = json!({"a": 1, "b": 2});
        assert_eq!(eval(&doc, "[]"), vec![&json!(1), &json!(2)]);
    }

    #[test]
    fn indexes_select_by_position_and_from_the_end() {
        let doc = json!({"ns": [10, 20, 30]});
        assert_eq!(eval(&doc, "ns[0]"), vec![&json!(10)]);
        assert_eq!(eval(&doc, ".ns[1]"), vec![&json!(20)]);
        assert_eq!(eval(&doc, "ns[-1]"), vec![&json!(30)]);
        assert!(eval(&doc, "ns[3]").is_empty());
        assert!(eval(&doc, "ns[-4]").is_empty());
    }

    #[test]
    fn quoted_fields_allow_dots_and_brackets_in_keys() {
        let doc = json!({"a.b": {"c d": 1}, "plain": 2});
        assert_eq!(eval(&doc, "\"a.b\".\"c d\""), vec![&json!(1)]);
        assert_eq!(eval(&doc, "[\"a.b\"][\"c d\"]"), vec![&json!(1)]);
        assert_eq!(eval(&doc, ".\"a.b\".\"c d\""), vec![&json!(1)]);
    }

    #[test]
    fn pipes_compose_like_dots() {
        let doc = json!({"a": {"items": [1, 2]}});
        assert_eq!(eval(&doc, ".a | .items | .[]"), vec![&json!(1), &json!(2)]);
    }

    #[test]
    fn optional_marker_is_accepted_and_ignored() {
        let doc = json!({"a": {"b": 5}});
        assert_eq!(eval(&doc, ".a?.b?"), vec![&json!(5)]);
        assert!(eval(&doc, ".a.b[]?").is_empty());
    }

    #[test]
    fn descend_enumerates_self_then_descendants_preorder() {
        let doc = json!({"x": [1, {"y": 2}]});
        let m = eval(&doc, "..");
        assert_eq!(
            m,
            vec![
                &doc,
                &json!([1, {"y": 2}]),
                &json!(1),
                &json!({"y": 2}),
                &json!(2),
            ]
        );
    }

    #[test]
    fn descend_then_field_finds_matches_at_any_depth() {
        let doc = json!({"a": {"status": "plan", "sub": {"status": "done"}}, "list": [{"status": "x"}]});
        let m = eval(&doc, ".. | .status?");
        assert_eq!(m, vec![&json!("plan"), &json!("done"), &json!("x")]);
        // Sugar form without the pipe.
        assert_eq!(eval(&doc, "..status"), m);
    }

    #[test]
    fn malformed_paths_are_rejected_not_guessed() {
        let doc = json!({"a": [1, 2, 3]});
        for bad in ["a[", "a[1", "a[1:2]", "a[\"x]", "\"unclosed", "a]", "a[b]"] {
            assert!(
                eval_json_path(&doc, bad).is_none(),
                "expected malformed: {}",
                bad
            );
        }
    }
}

#[cfg(test)]
mod eval_query_tests {
    use super::eval_query;
    use serde_json::json;

    // ── JMESPath dialect (the new power) ──

    #[test]
    fn multi_select_hash_projects_several_fields_per_record() {
        // The exact shape conversation 131 needed: one query, all fields.
        let doc = json!([
            {"name": "a", "full_name": "o/a", "private": false},
            {"name": "b", "full_name": "o/b", "private": true},
        ]);
        let out = eval_query(&doc, "[].{n: name, p: private}").unwrap();
        assert_eq!(
            out,
            vec![json!({"n": "a", "p": false}), json!({"n": "b", "p": true})]
        );
    }

    #[test]
    fn bare_field_projection_pages_element_by_element() {
        let doc = json!([{"full_name": "o/a"}, {"full_name": "o/b"}]);
        // An array result flattens into the match list (2 matches, not 1).
        assert_eq!(
            eval_query(&doc, "[].full_name").unwrap(),
            vec![json!("o/a"), json!("o/b")]
        );
    }

    #[test]
    fn filters_and_functions_are_available() {
        let doc = json!([{"n": "a", "on": true}, {"n": "b", "on": false}]);
        assert_eq!(
            eval_query(&doc, "[?on].n").unwrap(),
            vec![json!("a")]
        );
        assert_eq!(eval_query(&doc, "length(@)").unwrap(), vec![json!(2)]);
    }

    #[test]
    fn scalar_result_is_a_single_match() {
        let doc = json!({"a": {"b": 7}});
        assert_eq!(eval_query(&doc, "a.b").unwrap(), vec![json!(7)]);
    }

    #[test]
    fn jmespath_no_match_is_empty_not_error() {
        let doc = json!({"a": 1});
        assert!(eval_query(&doc, "missing").unwrap().is_empty());
    }

    // ── Legacy jq-selection fallback (unchanged behavior) ──

    #[test]
    fn leading_dot_paths_route_to_legacy_engine() {
        let doc = json!({"items": [{"s": "x"}, {"s": "y"}]});
        // Leading `.` is a JMESPath syntax error → legacy engine handles it.
        assert_eq!(
            eval_query(&doc, ".items[].s").unwrap(),
            vec![json!("x"), json!("y")]
        );
    }

    #[test]
    fn recursive_descent_still_works_via_fallback() {
        // JMESPath has no recursive descent; the legacy engine keeps `..`.
        let doc = json!({"a": {"status": "plan", "sub": {"status": "done"}}});
        assert_eq!(
            eval_query(&doc, ".. | .status?").unwrap(),
            vec![json!("plan"), json!("done")]
        );
    }

    #[test]
    fn pipes_and_optional_markers_route_to_legacy() {
        let doc = json!({"a": {"items": [1, 2]}});
        assert_eq!(
            eval_query(&doc, ".a | .items | .[]").unwrap(),
            vec![json!(1), json!(2)]
        );
    }

    // ── Both engines reject → loud error ──

    #[test]
    fn a_query_valid_in_neither_engine_errors() {
        let doc = json!({"a": 1});
        // Unclosed bracket: not JMESPath, not a legal selection path.
        assert!(eval_query(&doc, "a[").is_err());
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
        t if t == OBJ_TAG_SECRET => {
            // Secret handle (plan 132): serialize the redaction, never a
            // value. Secrets are not serializable by design — this keeps
            // stringify from being a laundering channel while making the
            // mistake visible in the output.
            let end = addr.saturating_add(8).saturating_add(count);
            let bytes = if end <= data.len() {
                &data[addr + 8..end]
            } else {
                &[][..]
            };
            let name = String::from_utf8_lossy(bytes);
            write_json_string(&format!("«secret {}»", name), out);
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
