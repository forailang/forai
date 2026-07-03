use super::*;

// ── $rt_print_val(val: i64) -> void ──────────────────────────────
// Writes value as string to linear memory at heap_ptr, calls env.print

pub(super) fn emit_print_val(base: u32, import_remap: &[Option<u32>]) -> Function {
    // Param 0: val (i64) from type sig
    // Extra locals: 1=ptr(i32), 2=len(i32)
    let locals = vec![
        (1, ValType::I32), // local 1: ptr
        (1, ValType::I32), // local 2: len
    ];
    let mut f = Function::new(locals);

    // ptr = heap_ptr (global 0)
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::LocalSet(1));

    // Check type and write appropriate string
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Int: call itoa, get length
        f.instruction(&Instruction::LocalGet(1)); // ptr (dest)
        f.instruction(&Instruction::LocalGet(0)); // val
        f.instruction(&Instruction::I32WrapI64); // extract i32
        f.instruction(&Instruction::Call(base + RT_ITOA));
        f.instruction(&Instruction::LocalSet(2)); // len = itoa result

        // Call env.print(ptr, len)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(0)); // import 0 = env.print
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Check for bool true
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(VAL_TRUE));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Write "true" to memory
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0x65757274)); // "true" in little-endian
        f.instruction(&Instruction::I32Store(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(4));
        emit_import_call(&mut f, IMPORT_PRINT, import_remap);
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Check for bool false
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(VAL_FALSE));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Write "false" to memory - 5 bytes
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0x736C6166)); // "fals" in little-endian
        f.instruction(&Instruction::I32Store(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0x65)); // "e"
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(5));
        emit_import_call(&mut f, IMPORT_PRINT, import_remap);
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Check for null
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Write "null"
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0x6C6C756E)); // "null" in little-endian
        f.instruction(&Instruction::I32Store(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::Call(0));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Check for float (not NaN-boxed tagged)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_FLOAT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // For M1: print float as int (truncated).
        // TODO: proper float-to-string in M2
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::F64ReinterpretI64);
        f.instruction(&Instruction::I32TruncSatF64S);
        f.instruction(&Instruction::Call(base + RT_ITOA));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        emit_import_call(&mut f, IMPORT_PRINT, import_remap);
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Default: print nothing (void, unknown types)
    f.instruction(&Instruction::End);
    f
}

// ── $rt_copy_deep(v: i64) -> i64 ──────────────────────────────────
// Deep-copy an object graph into FRESH OWNED blocks: allocate a new block the
// same size/tag, copy the header + string bytes, and recursively copy each
// pointer child into the new block. The result has no SHARED_BIT — it's a fully
// independent value that follows normal scope/ownership rules (the `copy(x)`
// builtin). Primitives are immediate (returned as-is). Unsizeable tags
// (closure/module/native) can't be copied — returned as-is (shared). Acyclic
// under single ownership, so the recursion terminates.
pub(super) fn emit_copy_deep(base: u32) -> Function {
    // param 0: v. locals 1=src,2=tag,3=count,4=i,5=size,6=dst,7=srcE,8=dstE.
    let mut f = Function::new([(8, ValType::I32)]);
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let b8 = MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    };
    let empty = wasm_encoder::BlockType::Empty;

    // if !is_obj(v) { return v }
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // src = obj_addr(v); tag = mem[src]; count = mem[src+4]; size = 0
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::LocalSet(3));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5));

    // size by tag (mirrors rt_drop_deep)
    let set_size = |f: &mut Function, hdr: i32, per: i32| {
        f.instruction(&Instruction::I32Const(hdr));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(per));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
    };
    // STRING → 8 + count*1
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    set_size(&mut f, 8, 1);
    f.instruction(&Instruction::End);
    // ARRAY||TUPLE → 8 + count*8
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_TUPLE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(empty));
    set_size(&mut f, 8, 8);
    f.instruction(&Instruction::End);
    // DICT → 8 + count*16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    set_size(&mut f, 8, 16);
    f.instruction(&Instruction::End);
    // INSTANCE → 16 + count*16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    set_size(&mut f, 16, 16);
    f.instruction(&Instruction::End);

    // if size == 0 { return v }  — unsizeable tag (closure/module/native)
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // dst = rt_alloc(size); mem[dst]=tag; mem[dst+4]=count
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(6));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Store(off4));

    // STRING → byte-copy `count` bytes (src+8 → dst+8)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::Block(empty));
    f.instruction(&Instruction::Loop(empty));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(b8));
    f.instruction(&Instruction::I32Store8(b8));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // recursively copy each pointer child from src entry to dst entry.
    let emit_copy_children =
        |f: &mut Function, entry_base: i32, stride: i32, child_offsets: &[u64]| {
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::LocalSet(4)); // i = 0
            f.instruction(&Instruction::Block(empty));
            f.instruction(&Instruction::Loop(empty));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));
            // srcE = src + entry_base + i*stride ; dstE = dst + entry_base + i*stride
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Const(entry_base));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(stride));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(7));
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(entry_base));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(stride));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(8));
            for &co in child_offsets {
                let ma = MemArg {
                    offset: co,
                    align: 0,
                    memory_index: 0,
                };
                // mem[dstE+co] = copy_deep(mem[srcE+co])
                f.instruction(&Instruction::LocalGet(8));
                f.instruction(&Instruction::LocalGet(7));
                f.instruction(&Instruction::I64Load(ma));
                f.instruction(&Instruction::Call(base + RT_COPY_DEEP));
                f.instruction(&Instruction::I64Store(ma));
            }
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::Br(0));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::End);
        };

    // ARRAY||TUPLE → child i64 @ +8, stride 8
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_TUPLE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(empty));
    emit_copy_children(&mut f, 8, 8, &[0]);
    f.instruction(&Instruction::End);
    // DICT → (key,val) @ +8, stride 16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    emit_copy_children(&mut f, 8, 16, &[0, 8]);
    f.instruction(&Instruction::End);
    // INSTANCE → copy header slot @ +8 (type metadata, shallow), entries @ +16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Load(b8));
    f.instruction(&Instruction::I64Store(b8));
    emit_copy_children(&mut f, 16, 16, &[0, 8]);
    f.instruction(&Instruction::End);

    // return make_obj(dst)
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_make_obj(addr: i32) -> i64 ───────────────────────────────
// NaN-box an address as an object pointer: QNAN | SIGN_BIT | addr

pub(super) fn emit_make_obj() -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64ExtendI32U);
    f.instruction(&Instruction::I64Const(QNAN | SIGN_BIT));
    f.instruction(&Instruction::I64Or);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_obj_addr(val: i64) -> i32 ────────────────────────────────
// Extract the 32-bit address from a NaN-boxed object pointer.

pub(super) fn emit_obj_addr() -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(0x0000_FFFF_FFFF_FFFF_u64 as i64));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_is_obj(val: i64) -> i32 ──────────────────────────────────
// Check if value is an object pointer (QNAN | SIGN_BIT set, not other tags).

pub(super) fn emit_is_obj() -> Function {
    let mut f = Function::new([]);
    // An object has QNAN | SIGN_BIT in the high bits
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const((QNAN | SIGN_BIT) as i64));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I64Const((QNAN | SIGN_BIT) as i64));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::End);
    f
}

// ══════════════════════════════════════════════════════════════════
// Phase 2.2: WASM-native runtime functions
// ══════════════════════════════════════════════════════════════════

// ── $rt_str_eq(a_ptr: i32, a_len: i32, b_ptr: i32, b_len: i32) -> i32 ──
pub(super) fn emit_str_eq() -> Function {
    let mut f = Function::new([(1, ValType::I32)]); // local 4: i (loop counter)
                                                    // Check lengths first
    f.instruction(&Instruction::LocalGet(1)); // a_len
    f.instruction(&Instruction::LocalGet(3)); // b_len
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // Compare byte-by-byte
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(4)); // i = 0
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        // if i >= a_len: return 1 (equal)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // if mem[a_ptr+i] != mem[b_ptr+i]: return 0
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // i++
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block
    f.instruction(&Instruction::I32Const(1)); // unreachable but needed for validation
    f.instruction(&Instruction::End);
    f
}

// ── $rt_str_cmp(a_ptr: i32, a_len: i32, b_ptr: i32, b_len: i32) -> i32 ──
// Returns -1 when a < b, 0 when equal, 1 when a > b.
pub(super) fn emit_str_cmp() -> Function {
    let mut f = Function::new([(3, ValType::I32)]); // 4=i, 5=min_len, 6=byte_diff
                                                    // min_len = min(a_len, b_len)
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I32,
    )));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalSet(5));

    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(4)); // i = 0
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        // if i >= min_len break
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));

        // byte_diff = a[i] - b[i]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(6));

        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::Br(1));
        }
        f.instruction(&Instruction::End);

        // non-zero byte diff decides ordering
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Shared prefix: shorter string sorts first.
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_get_index(obj: i64, idx: i64) -> i64 ──
pub(super) fn emit_get_index(base: u32) -> Function {
    // locals: 2=addr(i32), 3=len(i32), 4=i(i32),
    //         5=key_addr(i32), 6=key_len(i32)
    //
    // Polymorphic indexing matches the VM:
    //   - Array/Tuple: positional access (negative-index supported).
    //   - String key on Dict/Instance/Module/String: delegate to
    //     RT_GET_FIELD with the unboxed name pointer/length. This is
    //     load-bearing for module field access when the field name's
    //     string-pool index overflows u8 and the compiler falls back
    //     from Op::GetField to LoadString + Op::GetIndex (see
    //     fai-compiler/src/compiler.rs::emit_get_field). Without this
    //     path, e.g. `string.split(...)` in a large program silently
    //     returned null and downstream calls produced garbage —
    //     plans/bug-wasm-diff-insert-crash.md bug A.
    let mut f = Function::new([
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
    ]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(2));
    // len = mem[addr+4]
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3));
    // i = idx as i32
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::LocalSet(4));
    // Check tag is array(1) or tuple(2)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(3)); // tag < 3 means string/array/tuple
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(0)); // tag > 0 means not string
    f.instruction(&Instruction::I32GtU);
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Handle negative index
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::End);
        // Bounds check
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Else);
        {
            // Return mem[addr + 8 + i*8]
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I64Load(mem0()));
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::Else);
    {
        // Not array/tuple. If idx is a string object, delegate to
        // RT_GET_FIELD so module/dict/instance/string lookups behave
        // the same as a literal-name field access.
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_IS_OBJ));
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            // key_addr = obj_addr(idx); tag = mem[key_addr]
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
            f.instruction(&Instruction::LocalSet(5));
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32Load(mem0()));
            f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
            f.instruction(&Instruction::I32Eq);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                ValType::I64,
            )));
            {
                // key_len = mem[key_addr+4]
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I32Load(MemArg {
                    offset: 4,
                    align: 0,
                    memory_index: 0,
                }));
                f.instruction(&Instruction::LocalSet(6));
                // get_field(obj, key_addr+8, key_len)
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(6));
                f.instruction(&Instruction::Call(base + RT_GET_FIELD));
            }
            f.instruction(&Instruction::Else);
            f.instruction(&Instruction::I64Const(VAL_NULL));
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_import_module(name_ptr: i32, name_len: i32) -> i64 ──
pub(super) fn emit_import_module(base: u32) -> Function {
    let mut f = Function::new([(1, ValType::I32)]); // local 2: addr
                                                    // Allocate [tag=5][name_ptr][name_len] = 12 bytes, padded to 16
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_MODULE));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(0)); // name_ptr
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(1)); // name_len
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_get_field(obj: i64, name_ptr: i32, name_len: i32) -> i64 ──
pub(super) fn emit_get_field(base: u32, ks: &KnownStrings) -> Function {
    // locals: 3=addr(i32), 4=tag(i32), 5=count(i32), 6=i(i32),
    //         7=entry_addr(i32), 8=key_addr(i32), 9=key_len(i32), 10=method_id(i32), 11=fn_addr(i32)
    let mut f = Function::new([
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
    ]);
    // addr = obj_addr(obj)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(3));
    // tag = mem[addr]
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(4));

    // === Dict / Instance path ===
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // count = mem[addr+4]
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(5));
        // entry base = 8 for dict, 16 for instance
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6)); // i = 0
                                                  // Loop over entries
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if i >= count: break with VAL_NULL
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::I64Const(VAL_NULL));
            f.instruction(&Instruction::Br(2)); // break out of block with value
            f.instruction(&Instruction::End);
            // entry_addr = addr + entry_base + i*16
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(7)); // entry_addr
                                                      // key_val = mem[entry_addr] as i64, extract string addr
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
            f.instruction(&Instruction::LocalSet(8)); // key_addr
                                                      // key_len = mem[key_addr+4]
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32Load(MemArg {
                offset: 4,
                align: 0,
                memory_index: 0,
            }));
            f.instruction(&Instruction::LocalSet(9));
            // Compare: str_eq(key_addr+8, key_len, name_ptr, name_len)
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::LocalGet(1)); // name_ptr
            f.instruction(&Instruction::LocalGet(2)); // name_len
            f.instruction(&Instruction::Call(base + RT_STR_EQ));
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            {
                // Match! Return mem[entry_addr+8]
                f.instruction(&Instruction::LocalGet(7));
                f.instruction(&Instruction::I64Load(MemArg {
                    offset: 8,
                    align: 0,
                    memory_index: 0,
                }));
                f.instruction(&Instruction::Br(3)); // break out of block with value
            }
            f.instruction(&Instruction::End);
            // i++
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(6));
            f.instruction(&Instruction::Br(0)); // continue loop
        }
        f.instruction(&Instruction::End); // end loop
        f.instruction(&Instruction::I64Const(VAL_NULL)); // fallback
        f.instruction(&Instruction::End); // end block
    }
    f.instruction(&Instruction::Else);
    {
        // === Module path (tag=5) ===
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(OBJ_TAG_MODULE));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            // Resolve method_id from module_name + method_name
            // method_id defaults to UNKNOWN
            f.instruction(&Instruction::I32Const(METHOD_UNKNOWN));
            f.instruction(&Instruction::LocalSet(10));

            // Check "length" method (works for all modules)
            emit_method_check(&mut f, base, ks.length, 1, 2, METHOD_LENGTH, 10);
            // Check "abs"
            emit_method_check(&mut f, base, ks.abs, 1, 2, METHOD_ABS, 10);
            // Check "min"
            emit_method_check(&mut f, base, ks.min, 1, 2, METHOD_MIN, 10);
            // Check "max"
            emit_method_check(&mut f, base, ks.max, 1, 2, METHOD_MAX, 10);
            // Check "floor"
            emit_method_check(&mut f, base, ks.floor, 1, 2, METHOD_FLOOR, 10);
            // Check "ceil"
            emit_method_check(&mut f, base, ks.ceil, 1, 2, METHOD_CEIL, 10);
            // Check "round"
            emit_method_check(&mut f, base, ks.round, 1, 2, METHOD_ROUND, 10);
            // Check "sqrt"
            emit_method_check(&mut f, base, ks.sqrt, 1, 2, METHOD_SQRT, 10);
            // Check "contains"
            emit_method_check(&mut f, base, ks.contains, 1, 2, METHOD_CONTAINS, 10);
            // Check "split"
            emit_method_check(&mut f, base, ks.split, 1, 2, METHOD_SPLIT, 10);
            // Check "join"
            emit_method_check(&mut f, base, ks.join, 1, 2, METHOD_JOIN, 10);
            // Check "sort"
            emit_method_check(&mut f, base, ks.sort, 1, 2, METHOD_SORT, 10);
            // Check "getKeys"
            emit_method_check(&mut f, base, ks.get_keys, 1, 2, METHOD_GET_KEYS, 10);
            // Check "slice"
            emit_method_check(&mut f, base, ks.slice, 1, 2, METHOD_SLICE, 10);
            // Check "reverse"
            emit_method_check(&mut f, base, ks.reverse, 1, 2, METHOD_REVERSE, 10);
            // Check "toUpper"
            emit_method_check(&mut f, base, ks.to_upper, 1, 2, METHOD_TO_UPPER, 10);
            // Check "toLower"
            emit_method_check(&mut f, base, ks.to_lower, 1, 2, METHOD_TO_LOWER, 10);
            // Check "trim"
            emit_method_check(&mut f, base, ks.trim, 1, 2, METHOD_TRIM, 10);
            // Check "startsWith"
            emit_method_check(&mut f, base, ks.starts_with, 1, 2, METHOD_STARTS_WITH, 10);
            // Check "endsWith"
            emit_method_check(&mut f, base, ks.ends_with, 1, 2, METHOD_ENDS_WITH, 10);
            // Check "indexOf"
            emit_method_check(&mut f, base, ks.index_of, 1, 2, METHOD_INDEX_OF, 10);
            // Check "substring"
            emit_method_check(&mut f, base, ks.substring, 1, 2, METHOD_SUBSTRING, 10);
            // Check "repeat"
            emit_method_check(&mut f, base, ks.repeat, 1, 2, METHOD_REPEAT, 10);
            // Check "replace"
            emit_method_check(&mut f, base, ks.replace, 1, 2, METHOD_REPLACE, 10);
            // Check "pow"
            emit_method_check(&mut f, base, ks.pow, 1, 2, METHOD_POW, 10);
            // Check "append"
            emit_method_check(&mut f, base, ks.append, 1, 2, METHOD_APPEND, 10);
            // Check "isEmpty"
            emit_method_check(&mut f, base, ks.is_empty, 1, 2, METHOD_IS_EMPTY, 10);
            // Check "first" / "last"
            emit_method_check(&mut f, base, ks.first, 1, 2, METHOD_FIRST, 10);
            emit_method_check(&mut f, base, ks.last, 1, 2, METHOD_LAST, 10);
            // File/time/random/sleep methods
            emit_method_check(&mut f, base, ks.read, 1, 2, METHOD_FILE_READ, 10);
            emit_method_check(&mut f, base, ks.write, 1, 2, METHOD_FILE_WRITE, 10);
            emit_method_check(&mut f, base, ks.exists, 1, 2, METHOD_FILE_EXISTS, 10);
            emit_method_check(&mut f, base, ks.now, 1, 2, METHOD_TIME_NOW, 10);
            emit_method_check(&mut f, base, ks.unix, 1, 2, METHOD_TIME_UNIX, 10);
            emit_method_check(&mut f, base, ks.random, 1, 2, METHOD_RANDOM, 10);
            emit_method_check(&mut f, base, ks.sleep, 1, 2, METHOD_SLEEP, 10);
            // std.http.server methods. `listen` uses the router accept loop;
            // `text`/`html`/`json`/`ok`/`redirect` all build response dicts
            // via IMPORT_HTTP_SERVER_RESPONSE with different `kind`
            // discriminants (see RESPONSE_KIND_*).
            emit_method_check(&mut f, base, ks.listen, 1, 2, METHOD_SERVER_LISTEN, 10);
            emit_method_check(&mut f, base, ks.text, 1, 2, METHOD_SERVER_TEXT, 10);
            emit_method_check(&mut f, base, ks.html, 1, 2, METHOD_SERVER_HTML, 10);
            emit_method_check(&mut f, base, ks.json_fn, 1, 2, METHOD_SERVER_JSON, 10);
            emit_method_check(&mut f, base, ks.ok, 1, 2, METHOD_SERVER_OK, 10);
            emit_method_check(&mut f, base, ks.redirect, 1, 2, METHOD_SERVER_REDIRECT, 10);
            emit_method_check(&mut f, base, ks.router, 1, 2, METHOD_SERVER_ROUTER, 10);
            emit_method_check(&mut f, base, ks.get, 1, 2, METHOD_SERVER_GET, 10);
            emit_method_check(&mut f, base, ks.post, 1, 2, METHOD_SERVER_POST, 10);
            emit_method_check(
                &mut f,
                base,
                ks.serve_files,
                1,
                2,
                METHOD_SERVER_SERVE_FILES,
                10,
            );
            // std.dictionary typed accessors — all three share a body.
            emit_method_check(&mut f, base, ks.get_string, 1, 2, METHOD_GET_STRING, 10);
            emit_method_check(&mut f, base, ks.get_int, 1, 2, METHOD_GET_INT, 10);
            emit_method_check(&mut f, base, ks.get_bool, 1, 2, METHOD_GET_BOOL, 10);
            emit_method_check(&mut f, base, ks.trim_start, 1, 2, METHOD_TRIM_START, 10);
            emit_method_check(&mut f, base, ks.trim_end, 1, 2, METHOD_TRIM_END, 10);
            // std.json methods — the compile-time sentinel path in
            // emit_get_field_by_idx only triggers when the module reg's
            // origin is Global(...). For top-level files this is true,
            // but inside a library-compiled function (e.g. Forui.rpc's
            // parseFnName) the `json` reference is loaded from the
            // globals area via plain GetGlobal + GetField — falling
            // through to RT_GET_FIELD, which needs a real method_id.
            emit_method_check(&mut f, base, ks.parse, 1, 2, METHOD_JSON_PARSE, 10);
            emit_method_check(&mut f, base, ks.stringify, 1, 2, METHOD_JSON_STRINGIFY, 10);
            // std.storage methods
            emit_method_check(&mut f, base, ks.storage_get, 1, 2, METHOD_STORAGE_GET, 10);
            emit_method_check(&mut f, base, ks.storage_set, 1, 2, METHOD_STORAGE_SET, 10);
            emit_method_check(
                &mut f,
                base,
                ks.storage_remove,
                1,
                2,
                METHOD_STORAGE_REMOVE,
                10,
            );
            emit_method_check(
                &mut f,
                base,
                ks.storage_clear,
                1,
                2,
                METHOD_STORAGE_CLEAR,
                10,
            );

            // Allocate NativeFn: [tag=6][method_id] = 8 bytes padded to 16
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::Call(base + RT_ALLOC));
            f.instruction(&Instruction::LocalSet(11));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(OBJ_TAG_NATIVE_FN));
            f.instruction(&Instruction::I32Store(mem0()));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::LocalGet(10)); // method_id
            f.instruction(&Instruction::I32Store(MemArg {
                offset: 4,
                align: 0,
                memory_index: 0,
            }));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        }
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

/// Emit a `server.<kind>(...)` body that bundles up a response `kind`,
/// status, and body string, then calls `IMPORT_HTTP_SERVER_RESPONSE`
/// and returns its result.
///
/// - `body_is_arg1 = true` matches `text/html/json/redirect(status, body)`
///   where arg0 is the status Int and arg1 is the body String.
/// - `body_is_arg1 = false` matches `ok(body)` where arg0 is the body
///   String and status is hardcoded to 200.
///
/// Scratch locals used: 7 (body_addr), 8 (body_len), 9 (body_ptr).
/// These are within the i32 temp block reserved by emit_call_native.
pub(super) fn emit_server_response_call(
    f: &mut Function,
    _base: u32,
    kind: i32,
    body_is_arg1: bool,
    import_remap: &[Option<u32>],
) {
    let body_local = if body_is_arg1 { 6 } else { 5 };

    // body_addr = obj_addr(body_val)
    f.instruction(&Instruction::LocalGet(body_local));
    f.instruction(&Instruction::Call(_base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(7));
    // body_len = mem[body_addr + 4]
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(8));
    // body_ptr = body_addr + 8
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(9));

    // args: (kind, status, body_ptr, body_len)
    f.instruction(&Instruction::I32Const(kind));
    if body_is_arg1 {
        // status = (arg0 & 0xFFFFFFFF) as i32
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
    } else {
        f.instruction(&Instruction::I32Const(200));
    }
    f.instruction(&Instruction::LocalGet(9));
    f.instruction(&Instruction::LocalGet(8));
    emit_import_call(f, IMPORT_HTTP_SERVER_RESPONSE, import_remap);
    f.instruction(&Instruction::Return);
}

/// Emit a method name check: if str_eq(name_ptr, name_len, known_ptr, known_len) then method_id = id
pub(super) fn emit_method_check(
    f: &mut Function,
    base: u32,
    known: (u32, u32),
    name_ptr_local: u32,
    name_len_local: u32,
    method_id: i32,
    result_local: u32,
) {
    let (kptr, klen) = known;
    f.instruction(&Instruction::LocalGet(name_ptr_local));
    f.instruction(&Instruction::LocalGet(name_len_local));
    f.instruction(&Instruction::I32Const(kptr as i32));
    f.instruction(&Instruction::I32Const(klen as i32));
    f.instruction(&Instruction::Call(base + RT_STR_EQ));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(method_id));
    f.instruction(&Instruction::LocalSet(result_local));
    f.instruction(&Instruction::End);
}

// ── $rt_set_field(obj: i64, name_ptr: i32, name_len: i32, val: i64) -> void ──
pub(super) fn emit_set_field(base: u32, import_remap: &[Option<u32>]) -> Function {
    // locals: 4=addr(i32), 5=count(i32), 6=i(i32), 7=entry_addr(i32),
    //         8=key_addr(i32), 9=key_len(i32), 10=entry_base(i32),
    //         11=is_instance(i32), 12=cap(i32), 13=new_addr(i32),
    //         14=gi(i32 grow-copy index), 15=src_entry(i32),
    //         16=dst_entry(i32). Returns i64 (the dict pointer).
    let mut f = Function::new([(13, ValType::I32)]);
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let off8 = MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    };
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(4));
    // Accept tag == DICT (3) or INSTANCE (7). Mirrors RT_GET_FIELD's
    // combined dict/instance path. Before this covered Instance,
    // writes to Instance-tagged objects (produced by the
    // RT_CALL_NATIVE tuple-constructor path when a typedef is called
    // via a variable) were silently dropped.
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(0)); // not a dict/instance → return v unchanged
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // is_instance = (tag == 7)
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalSet(11));
    // entry_base = 16 for Instance (type_name at offset 8 occupies
    // 8 bytes, so entries start at 16), 8 for Dict.
    f.instruction(&Instruction::LocalGet(11));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::LocalSet(10));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalSet(10));
    f.instruction(&Instruction::End);
    // count = mem[addr+4]
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(6));
    // Loop
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1)); // break
                                              // entry_addr = addr + entry_base + i*16
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7));
        // key string comparison
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I64Load(mem0()));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(base + RT_STR_EQ));
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        {
            // Match: release the value this entry currently holds (the dict
            // owned it — RC, plan 113 R1; RT_RELEASE's is_obj guard skips a
            // primitive), then write the new value. The caller has already
            // retained `val` when it was a borrowed source.
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I64Load(MemArg {
                offset: 8,
                align: 0,
                memory_index: 0,
            }));
            f.instruction(&Instruction::Call(base + RT_RELEASE));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(3)); // val
            f.instruction(&Instruction::I64Store(off8));
            f.instruction(&Instruction::LocalGet(0)); // address unchanged → return v
            f.instruction(&Instruction::Return);
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    // Key not found. Only Dict grows — Instance has a fixed shape
    // defined by its typedef, so an unknown-field write is silently
    // ignored (matching the VM which also skips unknown instance
    // fields since the checker rejects them at compile time).
    f.instruction(&Instruction::LocalGet(11));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(0)); // instance: no append → return v
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // Dict append. The block was sized for `cap` entries
    // (`cap = (logical_size - 8) / 16`, logical size in the rc-prefix
    // word at addr-4). If it's full, grow: allocate a bigger block,
    // shallow-copy the header + entries, and RETAIN each moved key/value
    // so both blocks hold a ref. We do NOT free the old block here — the
    // caller's `var` reassignment releases the old dict (which recursively
    // releases its children, dropping them back to the count the new
    // block now owns). That leaves the new block's children correctly
    // owned; the only cost is the old header block leaking by one rc on
    // a grow (sound and bounded — far better than the silent heap
    // overflow this replaces).
    // cap = (mem[addr-4] - 8) / 16
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::I32DivU);
    f.instruction(&Instruction::LocalSet(12));
    // Sanity guard: a plausible dict capacity is small. A huge `cap`
    // means the size word at addr-4 was garbage (set() called on a
    // non-dict / stale / mis-typed pointer), and growing would request
    // gigabytes and exhaust memory. Trap with the bad capacity + size
    // word and a backtrace instead — names the caller passing the bad
    // value. (1<<24 = 16M entries — orders beyond any real dict.)
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::I32Const(1 << 24));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    emit_trap_report_unreachable(
        &mut f,
        import_remap,
        TRAP_DICT_CAP_INSANE,
        |f| {
            f.instruction(&Instruction::LocalGet(12)); // computed capacity
            f.instruction(&Instruction::I64ExtendI32U);
        },
        |f| {
            // raw size word at addr-4
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(4));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Load(mem0()));
            f.instruction(&Instruction::I64ExtendI32U);
        },
    );
    f.instruction(&Instruction::End);
    // if count >= cap: grow
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // new_addr = alloc(8 + (cap*2)*16). cap is always >= 16 for dicts
        // (literal floor), so cap*2 stays comfortably bounded.
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(32)); // 2 * 16
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(13));
        // header: tag + count
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Store(off4));
        // copy + retain each of `count` entries
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(14));
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));
            // src_entry = addr + 8 + gi*16; dst_entry = new_addr + 8 + gi*16
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(15));
            f.instruction(&Instruction::LocalGet(13));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(16));
            // dst.key = src.key; retain it
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::I64Store(mem0()));
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::Call(base + RT_RETAIN));
            f.instruction(&Instruction::Drop);
            // dst.val = src.val; retain it
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::I64Load(off8));
            f.instruction(&Instruction::I64Store(off8));
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::I64Load(off8));
            f.instruction(&Instruction::Call(base + RT_RETAIN));
            f.instruction(&Instruction::Drop);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(14));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End); // loop
        f.instruction(&Instruction::End); // block
                                          // addr = new_addr (subsequent append + return use the grown block)
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalSet(4));
    }
    f.instruction(&Instruction::End); // grow
                                      // Append new entry at addr + 8 + count*16.
    f.instruction(&Instruction::LocalGet(4)); // addr
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(5)); // count
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(7)); // entry_addr = addr + 8 + count*16
                                              // Write key (allocate string object)
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::LocalGet(1)); // name_ptr
    f.instruction(&Instruction::LocalGet(2)); // name_len
    f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
    f.instruction(&Instruction::I64Store(mem0())); // store key at entry_addr
                                                   // Write value
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::LocalGet(3)); // val
    f.instruction(&Instruction::I64Store(off8)); // store val at entry_addr+8
                                                 // Increment count
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store(off4));
    // Return the (possibly new) dict pointer, NaN-boxed.
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End); // function end
    f
}

// ── $rt_value_to_str(val: i64) -> i64 ──
pub(super) fn emit_value_to_str(base: u32, ks: &KnownStrings, import_remap: &[Option<u32>]) -> Function {
    // locals: 1=addr(i32), 2=len(i32)
    let mut f = Function::new([(1, ValType::I32), (1, ValType::I32)]);

    // If already a string object, return as-is
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);

    // Int: use itoa
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Reserve 32 bytes of scratch space by bumping heap_ptr
        f.instruction(&Instruction::I32Const(32));
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(1)); // scratch addr
                                                  // Write digits to scratch
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::Call(base + RT_ITOA));
        f.instruction(&Instruction::LocalSet(2)); // len
                                                  // Allocate string object from scratch
        f.instruction(&Instruction::LocalGet(1)); // src_ptr
        f.instruction(&Instruction::LocalGet(2)); // len
        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
        // Free the itoa scratch: alloc_string copied the bytes into a fresh
        // String, so the 32-byte scratch is dead. Returning it to the free list
        // lets the next int→str conversion reuse it instead of bumping the heap
        // every time (otherwise each `toString(n)` / `"" + n` leaks 32 bytes).
        // The result String is on the stack; rt_free pushes nothing.
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(32));
        f.instruction(&Instruction::Call(base + RT_FREE));
    }
    f.instruction(&Instruction::Else);
    {
        // Bool true
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I64Const(VAL_TRUE));
        f.instruction(&Instruction::I64Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            f.instruction(&Instruction::I32Const(ks.str_true.0 as i32));
            f.instruction(&Instruction::I32Const(ks.str_true.1 as i32));
            f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
        }
        f.instruction(&Instruction::Else);
        {
            // Bool false
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::I64Const(VAL_FALSE));
            f.instruction(&Instruction::I64Eq);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                ValType::I64,
            )));
            {
                f.instruction(&Instruction::I32Const(ks.str_false.0 as i32));
                f.instruction(&Instruction::I32Const(ks.str_false.1 as i32));
                f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
            }
            f.instruction(&Instruction::Else);
            {
                // Null
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::I64Const(VAL_NULL));
                f.instruction(&Instruction::I64Eq);
                f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                    ValType::I64,
                )));
                {
                    f.instruction(&Instruction::I32Const(ks.str_null.0 as i32));
                    f.instruction(&Instruction::I32Const(ks.str_null.1 as i32));
                    f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
                }
                f.instruction(&Instruction::Else);
                {
                    // Float: use host import for proper formatting
                    f.instruction(&Instruction::LocalGet(0));
                    f.instruction(&Instruction::Call(base + RT_IS_FLOAT));
                    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                        ValType::I64,
                    )));
                    {
                        // Allocate scratch buffer for the string
                        f.instruction(&Instruction::I32Const(64));
                        f.instruction(&Instruction::Call(base + RT_ALLOC));
                        f.instruction(&Instruction::LocalSet(1)); // buf_ptr
                                                                  // Call host: float_to_str(f64_value, buf_ptr) -> len
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::F64ReinterpretI64);
                        f.instruction(&Instruction::LocalGet(1));
                        emit_import_call(&mut f, IMPORT_FLOAT_TO_STR, import_remap);
                        f.instruction(&Instruction::LocalSet(2)); // len
                                                                  // Allocate string object from buffer
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
                        // Free the 64-byte float-format scratch (see int path
                        // above): alloc_string copied it out, so reclaim it for
                        // the next conversion instead of leaking it.
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::I32Const(64));
                        f.instruction(&Instruction::Call(base + RT_FREE));
                    }
                    f.instruction(&Instruction::Else);
                    {
                        // Default: empty string
                        f.instruction(&Instruction::I32Const(0));
                        f.instruction(&Instruction::I32Const(0));
                        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
                    }
                    f.instruction(&Instruction::End);
                }
                f.instruction(&Instruction::End);
            }
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_print_val_new(val: i64) -> void ──
pub(super) fn emit_print_val_new(base: u32, import_remap: &[Option<u32>]) -> Function {
    let mut f = Function::new([(1, ValType::I32), (1, ValType::I64)]);

    // Existing String values are borrowed by print; do not release them.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
            f.instruction(&Instruction::LocalSet(1));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Load(MemArg {
                offset: 4,
                align: 0,
                memory_index: 0,
            }));
            emit_import_call(&mut f, IMPORT_PRINT, import_remap);
            f.instruction(&Instruction::Return);
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);

    // Non-string values stringify to a fresh String owned by this helper.
    // Release it after env.print copies the bytes.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
    f.instruction(&Instruction::LocalTee(2));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    // ptr = addr + 8
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    // len = mem[addr+4]
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    // call env.print(ptr, len)  — import index 0
    emit_import_call(&mut f, IMPORT_PRINT, import_remap);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::Call(base + RT_RELEASE));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_call_native(callee: i64, args_ptr: i32, arg_count: i32) -> i64 ──
/// Emit a loop that retains every element of a freshly built array (RC, plan
/// 113 R1). A runtime array builder (`append`, `sort`, `slice`, `reverse`,
/// `getKeys`, …) shallow-copies element references out of its source(s); the
/// new array co-owns each, so it must retain them or releasing the source later
/// deep-frees elements this array still points at. `dst_local` holds the array
/// payload base (tag@0), `count_local` the element count, `idx_local` is a
/// scratch i32 the caller guarantees is free at this point. RT_RETAIN's is_obj
/// guard makes this a no-op for primitive elements.
pub(super) fn emit_retain_array_elems(
    f: &mut Function,
    base: u32,
    dst_local: u32,
    count_local: u32,
    idx_local: u32,
) {
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(idx_local));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::LocalGet(count_local));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(dst_local));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I64Load(mem0()));
    f.instruction(&Instruction::Call(base + RT_RETAIN));
    f.instruction(&Instruction::Drop);
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(idx_local));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
}
