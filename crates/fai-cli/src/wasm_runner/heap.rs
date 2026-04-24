//! Guest-heap writers: allocate strings, arrays, dicts, and decode closure headers.

use wasmtime::*;

use super::nan_box::{encode_object, OBJ_TAG_ARRAY, OBJ_TAG_CLOSURE, OBJ_TAG_DICT, OBJ_TAG_STRING};

/// Read the current `__heap_ptr` global as a u32.
fn heap_ptr(caller: &mut Caller<'_, ()>) -> u32 {
    let g = caller
        .get_export("__heap_ptr")
        .unwrap()
        .into_global()
        .unwrap();
    g.get(&mut *caller).unwrap_i32() as u32
}

/// Bump `__heap_ptr` to a new (8-byte aligned) value.
fn set_heap_ptr(caller: &mut Caller<'_, ()>, new_heap: u32) {
    let g = caller
        .get_export("__heap_ptr")
        .unwrap()
        .into_global()
        .unwrap();
    let _ = g.set(&mut *caller, Val::I32(new_heap as i32));
}

/// Round `n` up to the next multiple of 8.
fn align8(n: u32) -> u32 {
    (n + 7) & !7
}

/// Allocate a string on the guest heap and return it as a NaN-boxed pointer.
///
/// Layout: `[tag:i32=OBJ_TAG_STRING][len:i32][bytes...]`.
pub(crate) fn wasm_alloc_str(caller: &mut Caller<'_, ()>, mem: &Memory, s: &str) -> i64 {
    let bytes = s.as_bytes();
    let addr = heap_ptr(caller);
    let data = mem.data_mut(&mut *caller);
    data[addr as usize..addr as usize + 4].copy_from_slice(&OBJ_TAG_STRING.to_le_bytes());
    data[addr as usize + 4..addr as usize + 8].copy_from_slice(&(bytes.len() as i32).to_le_bytes());
    data[addr as usize + 8..addr as usize + 8 + bytes.len()].copy_from_slice(bytes);
    let new_heap = align8(addr + 8 + bytes.len() as u32);
    set_heap_ptr(caller, new_heap);
    encode_object(addr)
}

/// Allocate a JSON value on the guest heap, producing a NaN-boxed pointer.
///
/// Recurses for arrays and objects. Scalars are encoded inline.
pub(crate) fn build_value(
    caller: &mut Caller<'_, ()>,
    mem: &Memory,
    val: &serde_json::Value,
) -> i64 {
    match val {
        serde_json::Value::Null => super::nan_box::VAL_NULL,
        serde_json::Value::Bool(b) => {
            (super::nan_box::QNAN | super::nan_box::TAG_BOOL | if *b { 1 } else { 0 }) as i64
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                (super::nan_box::QNAN | super::nan_box::TAG_INT | (i as u32 as u64)) as i64
            } else if let Some(f) = n.as_f64() {
                f.to_bits() as i64
            } else {
                super::nan_box::VAL_NULL
            }
        }
        serde_json::Value::String(s) => wasm_alloc_str(caller, mem, s),
        serde_json::Value::Array(arr) => {
            let items: Vec<i64> = arr.iter().map(|v| build_value(caller, mem, v)).collect();
            let addr = heap_ptr(caller);
            let data = mem.data_mut(&mut *caller);
            data[addr as usize..addr as usize + 4].copy_from_slice(&OBJ_TAG_ARRAY.to_le_bytes());
            data[addr as usize + 4..addr as usize + 8]
                .copy_from_slice(&(items.len() as i32).to_le_bytes());
            for (i, item) in items.iter().enumerate() {
                data[addr as usize + 8 + i * 8..addr as usize + 16 + i * 8]
                    .copy_from_slice(&item.to_le_bytes());
            }
            let new_heap = align8(addr + 8 + items.len() as u32 * 8);
            set_heap_ptr(caller, new_heap);
            encode_object(addr)
        }
        serde_json::Value::Object(obj) => {
            let mut entries: Vec<(i64, i64)> = Vec::new();
            for (k, v) in obj {
                let kv = wasm_alloc_str(caller, mem, k);
                let vv = build_value(caller, mem, v);
                entries.push((kv, vv));
            }
            let addr = heap_ptr(caller);
            let cap = std::cmp::max(entries.len(), 16);
            let data = mem.data_mut(&mut *caller);
            data[addr as usize..addr as usize + 4].copy_from_slice(&OBJ_TAG_DICT.to_le_bytes());
            data[addr as usize + 4..addr as usize + 8]
                .copy_from_slice(&(entries.len() as i32).to_le_bytes());
            for (i, (k, v)) in entries.iter().enumerate() {
                let ea = addr as usize + 8 + i * 16;
                data[ea..ea + 8].copy_from_slice(&k.to_le_bytes());
                data[ea + 8..ea + 16].copy_from_slice(&v.to_le_bytes());
            }
            let new_heap = align8(addr + 8 + cap as u32 * 16);
            set_heap_ptr(caller, new_heap);
            encode_object(addr)
        }
    }
}

/// Parsed closure header.
///
/// `table_idx` is the index into `__indirect_function_table`.
/// `env_addr` is the guest pointer to the upvalue payload (always `addr + 16`).
#[derive(Debug, PartialEq)]
pub(super) struct ClosureHeader {
    pub(super) table_idx: u32,
    pub(super) env_addr: i32,
}

/// Decode a closure header from raw guest memory bytes, returning `None` for
/// wrong tags or out-of-bounds addresses.
///
/// Closure layout: `[tag:i32=OBJ_TAG_CLOSURE][table_idx:i32][uv_count:i32][pad:i32][upvalues...]`.
pub(super) fn decode_closure_header(data: &[u8], addr: usize) -> Option<ClosureHeader> {
    if addr.checked_add(16)? > data.len() {
        return None;
    }
    let tag = i32::from_le_bytes(data[addr..addr + 4].try_into().ok()?);
    if tag != OBJ_TAG_CLOSURE {
        return None;
    }
    let table_idx = i32::from_le_bytes(data[addr + 4..addr + 8].try_into().ok()?) as u32;
    let env_addr = (addr + 16) as i32;
    Some(ClosureHeader {
        table_idx,
        env_addr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_closure_bytes(tag: i32, table_idx: i32) -> Vec<u8> {
        let mut v = Vec::with_capacity(64);
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&table_idx.to_le_bytes());
        v.extend_from_slice(&0_i32.to_le_bytes()); // uv_count
        v.extend_from_slice(&0_i32.to_le_bytes()); // pad
                                                   // upvalue bytes
        v.extend_from_slice(&[0u8; 16]);
        v
    }

    #[test]
    fn test_decode_closure_header_valid() {
        let data = make_closure_bytes(OBJ_TAG_CLOSURE, 7);
        let h = decode_closure_header(&data, 0).unwrap();
        assert_eq!(h.table_idx, 7);
        assert_eq!(h.env_addr, 16);
    }

    #[test]
    fn test_decode_closure_header_wrong_tag() {
        let data = make_closure_bytes(OBJ_TAG_STRING, 7);
        assert!(decode_closure_header(&data, 0).is_none());
    }

    #[test]
    fn test_decode_closure_header_out_of_bounds() {
        let short = vec![0u8; 8];
        assert!(decode_closure_header(&short, 0).is_none());
    }

    #[test]
    fn test_decode_closure_header_offset_addr() {
        // Place a closure at offset 32 and leave garbage before it
        let mut data = vec![0xFF; 32];
        data.extend(make_closure_bytes(OBJ_TAG_CLOSURE, 3));
        let h = decode_closure_header(&data, 32).unwrap();
        assert_eq!(h.table_idx, 3);
        assert_eq!(h.env_addr, 48);
    }

    #[test]
    fn test_decode_closure_header_addr_overflow() {
        let data = vec![0u8; 32];
        // addr near usize::MAX would overflow addr + 16
        assert!(decode_closure_header(&data, usize::MAX - 4).is_none());
    }

    #[test]
    fn test_align8_rounds_up() {
        assert_eq!(align8(0), 0);
        assert_eq!(align8(1), 8);
        assert_eq!(align8(7), 8);
        assert_eq!(align8(8), 8);
        assert_eq!(align8(9), 16);
        assert_eq!(align8(15), 16);
        assert_eq!(align8(16), 16);
    }
}
