use super::*;

// ── $rt_alloc_string(src_ptr: i32, len: i32) -> i64 ──
pub(super) fn emit_alloc_string(base: u32) -> Function {
    let mut f = Function::new([(1, ValType::I32)]); // local 2: addr
                                                    // size = 8 + len
    f.instruction(&Instruction::LocalGet(1)); // len
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(2)); // addr
                                              // write tag=0
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Store(mem0()));
    // write len
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    // copy data: memory.copy(dst=addr+8, src=src_ptr, len=len)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(0)); // src_ptr
    f.instruction(&Instruction::LocalGet(1)); // len
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });
    // box as object
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_concat(a: i64, b: i64) -> i64 ──
pub(super) fn emit_concat_fn(base: u32) -> Function {
    // locals: 2=addr_a(i32), 3=len_a(i32), 4=addr_b(i32), 5=len_b(i32), 6=dst(i32)
    let mut f = Function::new([
        (1, ValType::I32), // 2: addr_a
        (1, ValType::I32), // 3: len_a
        (1, ValType::I32), // 4: addr_b
        (1, ValType::I32), // 5: len_b
        (1, ValType::I32), // 6: dst
    ]);
    // Extract string a
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(2)); // addr_a
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3)); // len_a
                                              // Extract string b
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(4)); // addr_b
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(5)); // len_b
                                              // Allocate result: 8 + len_a + len_b
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(6)); // dst
                                              // Write tag=0
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Store(mem0()));
    // Write total len
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    // Copy a's data: memory.copy(dst+8, addr_a+8, len_a)
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });
    // Copy b's data: memory.copy(dst+8+len_a, addr_b+8, len_b)
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });
    // Box as object
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_concat_move(a: i64, b: i64) -> i64 ──
// Assignment-position concat: the codegen emits this only for `s = s + x`
// where `s` is the same owned (or cell-bound) local being reassigned, so the
// pre-call value of `s` is dead once the call returns.
//
// Fast path — `a` is a uniquely owned string (rc == 1) whose block has spare
// capacity (logical-size stamp at obj-4, the same discipline dict growth and
// METHOD_APPEND_MOVE use): stringify `b`, memcpy its bytes onto the end of
// `a`, bump the length, and return `a` retained (+1 owned result; the
// caller's release of the old binding drops it back to 1).
//
// Grow path — unique string but full: allocate max(needed, 2 × stamp, 32),
// copy both halves, return the fresh block. RT_ALLOC stamps the
// over-allocated logical size, so subsequent in-place appends are amortized
// O(1) and RT_FREE returns the block at its true size.
//
// Fallback — `a` is not a string or is shared (rc > 1): defer to RT_ADD,
// which preserves the exact legacy semantics (numeric add, copy concat).
// A stringified `b` temp (when `b` wasn't already a string) is released
// after its bytes are copied on the fast/grow paths; the fallback keeps
// RT_ADD's existing temp behavior.
pub(super) fn emit_concat_move(base: u32) -> Function {
    // params: 0 = a (i64), 1 = b (i64)
    // locals: 2=addr_a(i32), 3=len_a(i32), 4=addr_bs(i32), 5=len_b(i32),
    //         6=cap-then-dst(i32), 7=needed(i32), 8=bs(i64)
    let mut f = Function::new([(6, ValType::I32), (1, ValType::I64)]);
    let empty = wasm_encoder::BlockType::Empty;
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let bs_local: u32 = 8;

    // a must be an object …
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::If(empty));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(2)); // addr_a
        // … tagged String, uniquely owned (rc at obj-8 == 1).
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(empty));
        {
            // bs = value_to_str(b) — b as-is when it is already a string,
            // a fresh owned string otherwise.
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
            f.instruction(&Instruction::LocalSet(bs_local));
            f.instruction(&Instruction::LocalGet(bs_local));
            f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
            f.instruction(&Instruction::LocalSet(4)); // addr_bs
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Load(off4));
            f.instruction(&Instruction::LocalSet(3)); // len_a
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Load(off4));
            f.instruction(&Instruction::LocalSet(5)); // len_b
            // needed = 8 + len_a + len_b
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(7));

            // Fast: needed fits the stamped logical size at obj-4.
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(4));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Load(mem0()));
            f.instruction(&Instruction::I32LeU);
            f.instruction(&Instruction::If(empty));
            {
                // memcpy(addr_a + 8 + len_a, addr_bs + 8, len_b). memory.copy
                // has memmove semantics, so `s = s + s` (addr_bs == addr_a,
                // adjacent ranges) is safe; len_b was read before any write.
                f.instruction(&Instruction::LocalGet(2));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(3));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(4));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::MemoryCopy {
                    src_mem: 0,
                    dst_mem: 0,
                });
                f.instruction(&Instruction::LocalGet(2));
                f.instruction(&Instruction::LocalGet(3));
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::I32Store(off4));
                // Release a freshly stringified temp; keep a borrowed b.
                f.instruction(&Instruction::LocalGet(bs_local));
                f.instruction(&Instruction::LocalGet(1));
                f.instruction(&Instruction::I64Ne);
                f.instruction(&Instruction::If(empty));
                f.instruction(&Instruction::LocalGet(bs_local));
                f.instruction(&Instruction::Call(base + RT_RELEASE));
                f.instruction(&Instruction::End);
                // Owned return: RT_RETAIN passes the value through.
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::Call(base + RT_RETAIN));
                f.instruction(&Instruction::Return);
            }
            f.instruction(&Instruction::End);

            // Grow: cap = max(needed, 2 × stamp, 32).
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(4));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Load(mem0()));
            f.instruction(&Instruction::I32Const(2));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::LocalSet(6)); // cap (reuse dst slot)
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32LtU);
            f.instruction(&Instruction::If(empty));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalSet(6));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(32));
            f.instruction(&Instruction::I32LtU);
            f.instruction(&Instruction::If(empty));
            f.instruction(&Instruction::I32Const(32));
            f.instruction(&Instruction::LocalSet(6));
            f.instruction(&Instruction::End);

            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::Call(base + RT_ALLOC));
            f.instruction(&Instruction::LocalSet(6)); // dst
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
            f.instruction(&Instruction::I32Store(mem0()));
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Store(off4));
            // copy a's bytes, then b's.
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });
            f.instruction(&Instruction::LocalGet(bs_local));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I64Ne);
            f.instruction(&Instruction::If(empty));
            f.instruction(&Instruction::LocalGet(bs_local));
            f.instruction(&Instruction::Call(base + RT_RELEASE));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
            f.instruction(&Instruction::Return);
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);

    // Fallback: exact legacy `+` semantics.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_ADD));
    f.instruction(&Instruction::End);
    f
}

/// Helper to emit a method dispatch case: if method_id == id, run body and return.
pub(super) fn emit_native_method_dispatch(
    f: &mut Function,
    base: u32,
    method_id: i32,
    body: impl FnOnce(&mut Function, u32),
) {
    f.instruction(&Instruction::LocalGet(4)); // method_id local
    f.instruction(&Instruction::I32Const(method_id));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    body(f, base);
    f.instruction(&Instruction::End);
}

/// Emit an ASCII case-shift method body (toUpper when `to_upper=true`,
/// toLower otherwise). Takes the source string as arg0 (local 5),
/// allocates a new string of the same length, and copies each byte
/// with a shift applied when it falls in the opposite ASCII letter
/// range.
///
/// Local allocation inside this method body:
///   local 7: src_addr (i32)
///   local 8: src_len  (i32)
///   local 9: dst_addr (i32)
///   local 10: i       (i32) — loop counter
///   local 11: byte    (i32) — current byte
pub(super) fn emit_string_case_shift(f: &mut Function, base: u32, method_id: i32, to_upper: bool) {
    // Bytes shifted by +/- 32 (the distance between 'a' and 'A').
    let (range_lo, range_hi, shift): (i32, i32, i32) = if to_upper {
        (b'a' as i32, b'z' as i32, -32) // a-z -> A-Z
    } else {
        (b'A' as i32, b'Z' as i32, 32) // A-Z -> a-z
    };

    emit_native_method_dispatch(f, base, method_id, |f, base| {
        // src_addr = obj_addr(arg0)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // src_len = mem[src_addr + 4]
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));

        // Allocate new string object: 8 header + len bytes, heap-aligned
        // by rt_alloc (which over-allocates to 8-byte multiples). We
        // need the raw layout: [tag=0:i32][len:i32][data...].
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(9));

        // Write header: tag=0 (OBJ_TAG_STRING), len=src_len.
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        // Byte-by-byte copy with ASCII case shift.
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(10)); // i = 0
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if i >= len, break
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));

            // byte = mem8[src_addr + 8 + i]
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Load8U(mem0()));
            f.instruction(&Instruction::LocalSet(11));

            // If byte in [range_lo, range_hi], add shift.
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(range_lo));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(range_hi));
            f.instruction(&Instruction::I32LeU);
            f.instruction(&Instruction::I32And);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(shift));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(11));
            f.instruction(&Instruction::End);

            // mem8[dst_addr + 8 + i] = byte
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Store8(mem0()));

            // i++
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(10));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });
}

/// Shared body for METHOD_TRIM / METHOD_TRIM_START / METHOD_TRIM_END.
/// `strip_start` advances the start index past leading ASCII whitespace;
/// `strip_end` retreats the end index past trailing ASCII whitespace.
/// Setting both true is the classic `trim`. Non-ASCII bytes are never
/// treated as whitespace.
pub(super) fn emit_trim_body(f: &mut Function, base: u32, strip_start: bool, strip_end: bool) {
    // src_addr / len
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(7));
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(8));

    // start = 0 (local 9)
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(9));

    if strip_start {
        // Advance start past leading whitespace.
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Load8U(mem0()));
            f.instruction(&Instruction::LocalSet(11));
            emit_is_ascii_ws(f, 11);
            f.instruction(&Instruction::I32Eqz);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(9));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
    }

    // end = len (local 10)
    f.instruction(&Instruction::LocalGet(8));
    f.instruction(&Instruction::LocalSet(10));

    if strip_end {
        // Retreat end past trailing whitespace (while end > start).
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32LeU);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Load8U(mem0()));
            f.instruction(&Instruction::LocalSet(11));
            emit_is_ascii_ws(f, 11);
            f.instruction(&Instruction::I32Eqz);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::LocalSet(10));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
    }

    // Allocate dst string sized (end - start).
    f.instruction(&Instruction::LocalGet(10));
    f.instruction(&Instruction::LocalGet(9));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(12));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(13));
    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(9));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });

    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::Return);
}

/// Push `1` onto the stack if the byte in `byte_local` is an ASCII
/// whitespace (0x09, 0x0A, 0x0B, 0x0C, 0x0D, or 0x20). Leaves the
/// result as an i32 on the stack. Used by METHOD_TRIM.
///
/// The set matches Rust's `u8::is_ascii_whitespace` plus 0x0B (VT) so
/// that VM semantics for ASCII whitespace stay byte-compatible.
pub(super) fn emit_is_ascii_ws(f: &mut Function, byte_local: u32) {
    // Start with `is_space = (byte == 0x20)`.
    f.instruction(&Instruction::LocalGet(byte_local));
    f.instruction(&Instruction::I32Const(0x20));
    f.instruction(&Instruction::I32Eq);
    // OR each of the control bytes 0x09..=0x0D.
    for b in [0x09, 0x0A, 0x0B, 0x0C, 0x0D] {
        f.instruction(&Instruction::LocalGet(byte_local));
        f.instruction(&Instruction::I32Const(b));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
    }
}

/// Compare `prefix_len` bytes from `text[offset..offset+prefix_len]`
/// against `prefix[0..prefix_len]`, emitting a boolean result onto
/// `local 11` via RT_MAKE_BOOL and returning.
///
/// Caller is expected to have already ruled out the length-too-short
/// case and ensured `offset + prefix_len <= text_len`.
///
/// Local usage: uses 11..=14 as scratch. Safe because callers pass
/// their main-body locals in 7..=10.
pub(super) fn emit_byte_compare_prefix(
    f: &mut Function,
    base: u32,
    text_addr_local: u32,
    prefix_addr_local: u32,
    prefix_len_local: u32,
    offset_local: u32,
) {
    // i = 0
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(12));
    // match = 1
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(13));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        // if i >= prefix_len break
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::LocalGet(prefix_len_local));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        // text[offset + i] vs prefix[i]
        f.instruction(&Instruction::LocalGet(text_addr_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(offset_local));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalGet(prefix_addr_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::Br(2));
        f.instruction(&Instruction::End);
        // i++
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(12));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
    f.instruction(&Instruction::Return);
}

/// Scan an Array for an item equal (by i64 bit pattern) to arg1.
/// Returns a VAL_BOOL via RT_MAKE_BOOL and `Return`s. Intended to run
/// inside an outer `if tag == OBJ_TAG_ARRAY` guard; the caller must
/// NOT fall through to subsequent String code after this helper.
///
/// Uses locals: 7 (arr_addr, already set by the guard), 8 (len),
/// 13 (i loop counter).
pub(super) fn emit_array_contains_body(f: &mut Function, base: u32) {
    // len = mem[arr_addr + 4]
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(8));

    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(13)); // i
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));

        // item (i64) = mem[arr_addr + 8 + i*8]
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I64Load(mem0()));
        // Compare to arg1 (local 6) via RT_EQ so heap-allocated values
        // (strings, dicts, arrays) match by content rather than by
        // pointer identity. Ints, floats, bools, and null still
        // resolve to the same bit-equality short-circuit RT_EQ uses
        // for non-object operands.
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_EQ));
        // RT_EQ returns a NaN-boxed Bool (VAL_TRUE / VAL_FALSE).
        // Compare against VAL_TRUE to get the i32 condition the loop
        // branch needs.
        f.instruction(&Instruction::I64Const(VAL_TRUE));
        f.instruction(&Instruction::I64Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
    f.instruction(&Instruction::Return);
}

/// Scan an Array for an item equal (by i64 bit pattern) to arg1.
/// Returns the first matching index as Int, or -1. Shaped to match
/// `emit_array_contains_body` — same guard contract (returns inside
/// the enclosing `if tag == OBJ_TAG_ARRAY`).
pub(super) fn emit_array_index_of_body(f: &mut Function, base: u32) {
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(8));

    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(13));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));

        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I64Load(mem0()));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    f.instruction(&Instruction::Return);
}

/// Compare `find_len` bytes at `text[offset..offset+find_len]` against
/// `find[0..find_len]`. Stores the result (0/1) in `flag_local`.
/// Does NOT emit a return — used inline by `METHOD_REPLACE` where the
/// outer logic continues based on the flag.
///
/// Caller must have ensured `offset + find_len <= text_len` before
/// calling (i.e. this helper is safe to read the bytes).
///
/// Uses only the provided locals + temporary `j_local` as a loop
/// counter. All locals must be i32.
pub(super) fn emit_byte_compare_flag(
    f: &mut Function,
    text_addr_local: u32,
    find_addr_local: u32,
    find_len_local: u32,
    offset_local: u32,
    flag_local: u32,
    j_local: u32,
) {
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(flag_local));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(j_local));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(j_local));
        f.instruction(&Instruction::LocalGet(find_len_local));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        // text byte at offset + j
        f.instruction(&Instruction::LocalGet(text_addr_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(offset_local));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(j_local));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        // find byte at j
        f.instruction(&Instruction::LocalGet(find_addr_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(j_local));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(flag_local));
        f.instruction(&Instruction::Br(2));
        f.instruction(&Instruction::End);
        // j++
        f.instruction(&Instruction::LocalGet(j_local));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(j_local));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
}

/// Clamp `start` and `end` to `[0, len]` (with signed comparisons so
/// negative inputs fold to 0) and then fold `end < start` up to `end = start`.
/// Mirrors the VM's `native_substring` and `native_array_slice` clamps.
pub(super) fn emit_clamp_range_to_len(f: &mut Function, start_local: u32, end_local: u32, len_local: u32) {
    // start = max(0, start)
    f.instruction(&Instruction::LocalGet(start_local));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32LtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(start_local));
    f.instruction(&Instruction::End);
    // start = min(len, start)
    f.instruction(&Instruction::LocalGet(start_local));
    f.instruction(&Instruction::LocalGet(len_local));
    f.instruction(&Instruction::I32GtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(len_local));
    f.instruction(&Instruction::LocalSet(start_local));
    f.instruction(&Instruction::End);
    // end = max(0, end)
    f.instruction(&Instruction::LocalGet(end_local));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32LtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(end_local));
    f.instruction(&Instruction::End);
    // end = min(len, end)
    f.instruction(&Instruction::LocalGet(end_local));
    f.instruction(&Instruction::LocalGet(len_local));
    f.instruction(&Instruction::I32GtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(len_local));
    f.instruction(&Instruction::LocalSet(end_local));
    f.instruction(&Instruction::End);
    // if end < start, end = start
    f.instruction(&Instruction::LocalGet(end_local));
    f.instruction(&Instruction::LocalGet(start_local));
    f.instruction(&Instruction::I32LtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(start_local));
    f.instruction(&Instruction::LocalSet(end_local));
    f.instruction(&Instruction::End);
}
