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

/// Ensure linear memory can hold guest heap writes through `end`.
///
/// Host-side guest-heap writers bump `__heap_ptr` and write directly into
/// linear memory, bypassing the wasm `rt_alloc` (which grows memory itself).
/// Every such writer MUST call this first, or a write near the memory boundary
/// runs past the end of the backing slice and panics. Shared so all host
/// allocators (here and in `http_server::alloc_dict`) grow uniformly.
pub(crate) fn ensure_heap_capacity(caller: &mut Caller<'_, ()>, mem: &Memory, end: usize) {
    const WASM_PAGE_SIZE: usize = 64 * 1024;
    let current = mem.data_size(&mut *caller);
    if end <= current {
        return;
    }
    let missing = end - current;
    let pages = missing.div_ceil(WASM_PAGE_SIZE) as u64;
    mem.grow(&mut *caller, pages)
        .expect("failed to grow wasm memory for host heap allocation");
}

/// Reserve `logical_size` bytes for a guest heap object and return its **logical**
/// pointer (where `tag@0` goes). Mirrors the guest `rt_alloc` refcount prefix
/// (plan 113): the real block carries an 8-byte rc prefix, rc is initialised to
/// 1, and the returned pointer is `base + 8` — so `tag@0`/`count@4`/payload
/// offsets are unchanged. Advances `__heap_ptr` and grows memory as needed.
pub(crate) fn reserve(caller: &mut Caller<'_, ()>, mem: &Memory, logical_size: usize) -> u32 {
    let base = heap_ptr(caller);
    let real_end = base as usize + 8 + logical_size;
    ensure_heap_capacity(caller, mem, real_end);
    let data = mem.data_mut(&mut *caller);
    data[base as usize..base as usize + 4].copy_from_slice(&1i32.to_le_bytes()); // rc = 1
    // Logical alloc size in the spare prefix word (obj_addr-4), matching the
    // guest `rt_alloc`: `rt_release` reads it to free the block at its true size
    // (dicts over-allocate spare capacity, so a count-derived size is wrong;
    // plan 115).
    data[base as usize + 4..base as usize + 8]
        .copy_from_slice(&(logical_size as i32).to_le_bytes());
    let logical = base + 8;
    set_heap_ptr(caller, align8(logical + logical_size as u32));
    logical
}

/// Bump the refcount prefix of `v` if it is a heap object (RC, plan 113 R1).
/// Host-side mirror of the guest `rt_retain`: the count lives in the 8-byte
/// prefix at `addr-8`. Primitive (non-object) NaN-box values carry no count and
/// are skipped. A host allocator that stores references it shares with another
/// owner (e.g. `filter` keeping source elements, `all` collecting results) must
/// call this per stored element, or releasing the other owner frees them early.
pub(crate) fn host_retain(data: &mut [u8], v: i64) {
    use super::nan_box::{ADDR_MASK, QNAN, SIGN_BIT};
    let u = v as u64;
    if (u & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return; // not an object pointer
    }
    let addr = (u & ADDR_MASK) as usize;
    if addr < 8 || addr + 4 > data.len() {
        return;
    }
    let rc_off = addr - 8;
    let rc = i32::from_le_bytes(data[rc_off..rc_off + 4].try_into().unwrap());
    data[rc_off..rc_off + 4].copy_from_slice(&(rc + 1).to_le_bytes());
}

/// Allocate a string on the guest heap and return it as a NaN-boxed pointer.
///
/// Layout: `[tag:i32=OBJ_TAG_STRING][len:i32][bytes...]` (behind an 8-byte rc prefix).
pub(crate) fn wasm_alloc_str(caller: &mut Caller<'_, ()>, mem: &Memory, s: &str) -> i64 {
    let bytes = s.as_bytes();
    let addr = reserve(caller, mem, 8 + bytes.len()) as usize;
    let data = mem.data_mut(&mut *caller);
    data[addr..addr + 4].copy_from_slice(&OBJ_TAG_STRING.to_le_bytes());
    data[addr + 4..addr + 8].copy_from_slice(&(bytes.len() as i32).to_le_bytes());
    data[addr + 8..addr + 8 + bytes.len()].copy_from_slice(bytes);
    encode_object(addr as u32)
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
            let addr = reserve(caller, mem, 8 + items.len() * 8) as usize;
            let data = mem.data_mut(&mut *caller);
            data[addr..addr + 4].copy_from_slice(&OBJ_TAG_ARRAY.to_le_bytes());
            data[addr + 4..addr + 8].copy_from_slice(&(items.len() as i32).to_le_bytes());
            for (i, item) in items.iter().enumerate() {
                data[addr + 8 + i * 8..addr + 16 + i * 8].copy_from_slice(&item.to_le_bytes());
            }
            encode_object(addr as u32)
        }
        serde_json::Value::Object(obj) => {
            let mut entries: Vec<(i64, i64)> = Vec::new();
            for (k, v) in obj {
                let kv = wasm_alloc_str(caller, mem, k);
                let vv = build_value(caller, mem, v);
                entries.push((kv, vv));
            }
            let cap = std::cmp::max(entries.len(), 16);
            let addr = reserve(caller, mem, 8 + cap * 16) as usize;
            let data = mem.data_mut(&mut *caller);
            data[addr..addr + 4].copy_from_slice(&OBJ_TAG_DICT.to_le_bytes());
            data[addr + 4..addr + 8].copy_from_slice(&(entries.len() as i32).to_le_bytes());
            for (i, (k, v)) in entries.iter().enumerate() {
                let ea = addr + 8 + i * 16;
                data[ea..ea + 8].copy_from_slice(&k.to_le_bytes());
                data[ea + 8..ea + 16].copy_from_slice(&v.to_le_bytes());
            }
            encode_object(addr as u32)
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
    /// Frame size from the header (offset 12). `0` for a sync closure (a plain
    /// `FaiFunc` the host can `call_indirect`); `> 0` marks an *async* closure
    /// (a resume fn) the host must spawn+drive via `__fai_drive_closure`.
    pub(super) frame_size: i32,
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
    let frame_size = i32::from_le_bytes(data[addr + 12..addr + 16].try_into().ok()?);
    let env_addr = (addr + 16) as i32;
    Some(ClosureHeader {
        table_idx,
        env_addr,
        frame_size,
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
