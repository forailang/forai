use super::*;

// ── $is_int(val: i64) -> i32 ──────────────────────────────────────

pub(super) fn emit_is_int() -> Function {
    let mut f = Function::new([]); // param 0 = val (from type sig)
                                   // (val & INT_CHECK_MASK) == INT_CHECK_EXPECT
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(INT_CHECK_MASK));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I64Const(INT_CHECK_EXPECT));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::End);
    f
}

// ── $is_float(val: i64) -> i32 ────────────────────────────────────

pub(super) fn emit_is_float() -> Function {
    let mut f = Function::new([]);
    // A value is float if (val & QNAN) != QNAN
    // (simplified: doesn't handle canonical NaN, but sufficient for M1)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(QNAN));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I64Const(QNAN));
    f.instruction(&Instruction::I64Ne);
    f.instruction(&Instruction::End);
    f
}

// ── $as_number(val: i64) -> f64 ───────────────────────────────────

pub(super) fn emit_as_number(base: u32) -> Function {
    let mut f = Function::new([]);
    // if is_int(val): convert i32 payload to f64
    // else: reinterpret bits as f64
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::F64,
    )));
    {
        // Int path: extract low 32 bits as signed i32, convert to f64
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::F64ConvertI32S);
    }
    f.instruction(&Instruction::Else);
    {
        // Float path: reinterpret i64 bits as f64
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::F64ReinterpretI64);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $make_int(x: i32) -> i64 ─────────────────────────────────────

pub(super) fn emit_make_int() -> Function {
    let mut f = Function::new([]);
    // QNAN | TAG_INT | (x as u32 as u64)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64ExtendI32U);
    f.instruction(&Instruction::I64Const(QNAN | TAG_INT));
    f.instruction(&Instruction::I64Or);
    f.instruction(&Instruction::End);
    f
}

// ── $make_float(x: f64) -> i64 ───────────────────────────────────

pub(super) fn emit_make_float() -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64ReinterpretF64);
    f.instruction(&Instruction::End);
    f
}

// ── $make_bool(x: i32) -> i64 ────────────────────────────────────

pub(super) fn emit_make_bool() -> Function {
    let mut f = Function::new([]);
    // QNAN | TAG_BOOL | (x as u64)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64ExtendI32U);
    f.instruction(&Instruction::I64Const(QNAN | TAG_BOOL));
    f.instruction(&Instruction::I64Or);
    f.instruction(&Instruction::End);
    f
}

// ── Binary arithmetic with int/float dispatch ─────────────────────

#[derive(Clone, Copy)]
pub(super) enum IntOp {
    Sub,
    Mul,
}

pub(super) fn emit_binop_int_float(base: u32, op: IntOp) -> Function {
    let mut f = Function::new([]); // params a, b from type sig
                                   // if both_int(a, b): int_op, make_int
                                   // else: as_number(a) op as_number(b), make_float
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Int path
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32WrapI64);
        match op {
            IntOp::Sub => f.instruction(&Instruction::I32Sub),
            IntOp::Mul => f.instruction(&Instruction::I32Mul),
        };
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        // Float path
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        match op {
            IntOp::Sub => f.instruction(&Instruction::F64Sub),
            IntOp::Mul => f.instruction(&Instruction::F64Mul),
        };
        f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_add with string concat support ────────────────────────────
// If both int: int add. If either is object: call IMPORT_CONCAT. Else: float add.

pub(super) fn emit_add_with_concat(base: u32) -> Function {
    let mut f = Function::new([]);
    // Check both int first
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Both int: add as i32
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        // Check if either is an object (string concat)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_IS_OBJ));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_IS_OBJ));
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            // String concat via host — first convert both to strings, then concat
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
            f.instruction(&Instruction::Call(base + RT_CONCAT));
        }
        f.instruction(&Instruction::Else);
        {
            // Float path
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
            f.instruction(&Instruction::F64Add);
            f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_div (always returns float) ────────────────────────────────

pub(super) fn emit_div(base: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::F64Div);
    f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_idiv (floor division → int) ──────────────────────────────

pub(super) fn emit_idiv(base: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::F64Div);
    f.instruction(&Instruction::F64Floor);
    f.instruction(&Instruction::I32TruncF64S);
    f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_mod ───────────────────────────────────────────────────────

pub(super) fn emit_mod_op(base: u32) -> Function {
    // Extra locals 2=a(f64), 3=b(f64) for the float path.
    let mut f = Function::new([(2, ValType::F64)]);
    // Both int? use i32 remainder. Else float.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32RemS);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        // Float mod. WASM has no f64.rem, so compute a - trunc(a/b) * b —
        // the truncated remainder, matching the i32.rem_s semantics of the
        // int path (sign follows the dividend; b == 0 yields NaN).
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::F64Div);
        f.instruction(&Instruction::F64Trunc);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::F64Sub);
        f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_pow ───────────────────────────────────────────────────────

pub(super) fn emit_pow(base: u32) -> Function {
    // Params: local 0=a(i64), local 1=b(i64) from type sig
    // Extra locals: 2=result(f64), 3=base_val(f64), 4=exponent(i32), 5=counter(i32)
    let locals = vec![
        (1, ValType::F64), // local 2: result
        (1, ValType::F64), // local 3: base_val
        (1, ValType::I32), // local 4: exponent
        (1, ValType::I32), // local 5: counter
    ];
    let mut f = Function::new(locals);

    // base_val = as_number(a)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::LocalSet(3));

    // exponent = as_number(b) as i32 (truncate)
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::I32TruncF64S);
    f.instruction(&Instruction::LocalSet(4));

    // result = 1.0
    f.instruction(&Instruction::F64Const(1.0));
    f.instruction(&Instruction::LocalSet(2));

    // counter = 0
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5));

    // loop: while counter < exponent
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1)); // break

        // result = result * base_val
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::LocalSet(2));

        // counter++
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));

        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block

    // If both were int and result fits, return int; else float
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32TruncF64S);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    }
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::End);
    f
}

// ── $rt_neg ───────────────────────────────────────────────────────

pub(super) fn emit_neg(base: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::F64ReinterpretI64);
        f.instruction(&Instruction::F64Neg);
        f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── Comparison operators ──────────────────────────────────────────

#[derive(Clone, Copy)]
pub(super) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

pub(super) fn emit_cmp(base: u32, op: CmpOp) -> Function {
    // locals: 2=addr_a(i32), 3=addr_b(i32)
    let mut f = Function::new([(1, ValType::I32), (1, ValType::I32)]);

    // Check if both are int
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Int comparison
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32WrapI64);
        match op {
            CmpOp::Eq => f.instruction(&Instruction::I32Eq),
            CmpOp::Ne => f.instruction(&Instruction::I32Ne),
            CmpOp::Lt => f.instruction(&Instruction::I32LtS),
            CmpOp::Le => f.instruction(&Instruction::I32LeS),
            CmpOp::Gt => f.instruction(&Instruction::I32GtS),
            CmpOp::Ge => f.instruction(&Instruction::I32GeS),
        };
        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
    }
    f.instruction(&Instruction::Else);
    {
        // Check if both are float
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_IS_FLOAT));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_IS_FLOAT));
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            // Float comparison
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::F64ReinterpretI64);
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::F64ReinterpretI64);
            match op {
                CmpOp::Eq => f.instruction(&Instruction::F64Eq),
                CmpOp::Ne => f.instruction(&Instruction::F64Ne),
                CmpOp::Lt => f.instruction(&Instruction::F64Lt),
                CmpOp::Le => f.instruction(&Instruction::F64Le),
                CmpOp::Gt => f.instruction(&Instruction::F64Gt),
                CmpOp::Ge => f.instruction(&Instruction::F64Ge),
            };
            f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
        }
        f.instruction(&Instruction::Else);
        {
            match op {
                CmpOp::Eq | CmpOp::Ne => {
                    // Check if both are objects (strings) — compare content
                    f.instruction(&Instruction::LocalGet(0));
                    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                    f.instruction(&Instruction::LocalGet(1));
                    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                    f.instruction(&Instruction::I32And);
                    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                        ValType::I64,
                    )));
                    {
                        // Both objects: extract string data and compare via RT_STR_EQ
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
                        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                        f.instruction(&Instruction::LocalSet(2)); // addr_a
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
                        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                        f.instruction(&Instruction::LocalSet(3)); // addr_b
                                                                  // RT_STR_EQ(ptr_a, len_a, ptr_b, len_b) -> i32
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::I32Const(8));
                        f.instruction(&Instruction::I32Add); // ptr_a
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::I32Load(MemArg {
                            offset: 4,
                            align: 0,
                            memory_index: 0,
                        })); // len_a
                        f.instruction(&Instruction::LocalGet(3));
                        f.instruction(&Instruction::I32Const(8));
                        f.instruction(&Instruction::I32Add); // ptr_b
                        f.instruction(&Instruction::LocalGet(3));
                        f.instruction(&Instruction::I32Load(MemArg {
                            offset: 4,
                            align: 0,
                            memory_index: 0,
                        })); // len_b
                        f.instruction(&Instruction::Call(base + RT_STR_EQ));
                        if let CmpOp::Ne = op {
                            f.instruction(&Instruction::I32Eqz); // invert for Ne
                        }
                        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
                    }
                    f.instruction(&Instruction::Else);
                    {
                        // Non-object, non-numeric: compare raw i64 bits
                        // (null==null, bool==bool)
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::LocalGet(1));
                        match op {
                            CmpOp::Eq => f.instruction(&Instruction::I64Eq),
                            _ => f.instruction(&Instruction::I64Ne),
                        };
                        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
                    }
                    f.instruction(&Instruction::End);
                }
                _ => {
                    // Ordering: strings compare lexicographically by
                    // byte sequence; everything else falls back to
                    // numeric coercion.
                    f.instruction(&Instruction::LocalGet(0));
                    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                    f.instruction(&Instruction::LocalGet(1));
                    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                    f.instruction(&Instruction::I32And);
                    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                        ValType::I64,
                    )));
                    {
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
                        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                        f.instruction(&Instruction::LocalSet(2)); // addr_a
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
                        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                        f.instruction(&Instruction::LocalSet(3)); // addr_b
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::I32Const(8));
                        f.instruction(&Instruction::I32Add); // ptr_a
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::I32Load(MemArg {
                            offset: 4,
                            align: 0,
                            memory_index: 0,
                        })); // len_a
                        f.instruction(&Instruction::LocalGet(3));
                        f.instruction(&Instruction::I32Const(8));
                        f.instruction(&Instruction::I32Add); // ptr_b
                        f.instruction(&Instruction::LocalGet(3));
                        f.instruction(&Instruction::I32Load(MemArg {
                            offset: 4,
                            align: 0,
                            memory_index: 0,
                        })); // len_b
                        f.instruction(&Instruction::Call(base + RT_STR_CMP));
                        f.instruction(&Instruction::I32Const(0));
                        match op {
                            CmpOp::Lt => f.instruction(&Instruction::I32LtS),
                            CmpOp::Le => f.instruction(&Instruction::I32LeS),
                            CmpOp::Gt => f.instruction(&Instruction::I32GtS),
                            CmpOp::Ge => f.instruction(&Instruction::I32GeS),
                            _ => unreachable!(),
                        };
                        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
                    }
                    f.instruction(&Instruction::Else);
                    {
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
                        match op {
                            CmpOp::Lt => f.instruction(&Instruction::F64Lt),
                            CmpOp::Le => f.instruction(&Instruction::F64Le),
                            CmpOp::Gt => f.instruction(&Instruction::F64Gt),
                            CmpOp::Ge => f.instruction(&Instruction::F64Ge),
                            _ => unreachable!(),
                        };
                        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
                    }
                    f.instruction(&Instruction::End);
                }
            }
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_itoa(ptr: i32, val: i32) -> i32 (length) ────────────────
// Writes decimal digits of val into memory at ptr. Returns length.

pub(super) fn emit_itoa() -> Function {
    // Params: 0=ptr(i32), 1=val(i32) from type sig
    // Extra locals: 2=len, 3=is_neg, 4=digit, 5=start, 6=end, 7=tmp
    let locals = vec![
        (1, ValType::I32), // local 2: len
        (1, ValType::I32), // local 3: is_neg
        (1, ValType::I32), // local 4: digit
        (1, ValType::I32), // local 5: start (for reversal)
        (1, ValType::I32), // local 6: end (for reversal)
        (1, ValType::I32), // local 7: tmp
    ];
    let mut f = Function::new(locals);

    // Handle 0 specially
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(0x30)); // '0'
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Handle negative
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32LtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(3)); // is_neg = 1
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(1)); // val = -val
                                                  // Write '-'
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(0x2D)); // '-'
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(2)); // len = 1
    }
    f.instruction(&Instruction::End);

    // Write digits in reverse order starting at ptr+len
    // start = len (position of first digit)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalSet(5));

    // Loop: while val > 0
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1)); // break if val == 0

        // digit = val % 10
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32RemU);
        f.instruction(&Instruction::LocalSet(4));

        // val = val / 10
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32DivU);
        f.instruction(&Instruction::LocalSet(1));

        // mem[ptr + len] = digit + '0'
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0x30));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));

        // len++
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));

        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block

    // Reverse the digits in place: swap mem[ptr+start..] with mem[ptr+len-1..]
    // start index, end = len - 1
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(6)); // end = len - 1

    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        // if start >= end, break
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));

        // tmp = mem[ptr + start]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(7));

        // mem[ptr + start] = mem[ptr + end]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));

        // mem[ptr + end] = tmp
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));

        // start++, end--
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(6));

        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block

    // Return len
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_parse_int(str_val: i64) -> i64 (NaN-boxed Int or Null) ────────
//
// Parse an optionally-signed decimal integer from a String. Returns
// VAL_NULL for any invalid input (non-digit bytes, empty after trim,
// out-of-range). Mirrors the VM's `s.trim().parse::<i32>()` semantics
// except errors produce Null rather than a runtime FaiError — fai's
// typical idiom is `parseInt(x)` used as an expression, and the wasm
// codegen doesn't have a clean error-path model.
pub(super) fn emit_parse_int(base: u32) -> Function {
    // locals 1..7: i32 scratch (str_addr, start, end, byte, negative, result, digit)
    let mut f = Function::new([(7, ValType::I32)]);

    // addr = obj_addr(arg0)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    // end = addr + 8 + len  (the one-past-last byte position relative
    // to the data start, but we'll actually track start/end as byte
    // INDICES within the string data — simpler).
    // Let `start` (local 2) and `end` (local 3) be byte offsets into
    // the data region starting at addr+8.
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3));

    // Trim leading whitespace.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        emit_is_ascii_ws(&mut f, 4);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Trim trailing whitespace.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32LeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        emit_is_ascii_ws(&mut f, 4);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // If empty after trim, return Null.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // Optional leading sign.
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5)); // negative = 0
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem0()));
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(b'-' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::End);
    // Also skip a leading '+'.
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(b'+' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::End);

    // Must have at least one digit remaining.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // Accumulate digits.
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(6)); // result
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        // byte = mem[addr+8+i]
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        // validate '0'..='9'
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'9' as i32));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // digit = byte - '0'
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(7));
        // result = result * 10 + digit
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(6));
        // i++
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Apply sign.
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(6));
    f.instruction(&Instruction::End);

    // Return NaN-boxed Int.
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_parse_float(str_val: i64) -> i64 (NaN-boxed Float or Null) ────
//
// Parse a decimal float of the form `[sign] integer-part [. fraction]`
// (no exponent notation). Returns Null on invalid input. Mirrors the
// VM's `s.trim().parse::<f64>()` for the simple-decimal subset that
// Rust's FromStr handles — but we don't support exponents (`1e5`).
//
// Algorithm: parse the integer part as f64 by accumulating digit-by-
// digit via `result = result * 10 + d`. Then if a `.` follows, parse
// the fractional part similarly while also accumulating a divisor
// (10^n) and finally add `frac / divisor` to the result.
pub(super) fn emit_parse_float(base: u32) -> Function {
    // locals: 1 str_addr(i32), 2 i(i32), 3 end(i32), 4 byte(i32),
    // 5 negative(i32), 6 digit(i32), 7 seen_digit(i32),
    // 8 int_part(f64), 9 frac_part(f64), 10 divisor(f64)
    let mut f = Function::new([(7, ValType::I32), (3, ValType::F64)]);

    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3));

    // Trim leading ws.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        emit_is_ascii_ws(&mut f, 4);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Trim trailing ws.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32LeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        emit_is_ascii_ws(&mut f, 4);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Empty after trim → null.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // Sign.
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem0()));
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(b'-' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(b'+' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::End);

    // Integer part: accumulate into local 8 (f64).
    f.instruction(&Instruction::F64Const(0.0));
    f.instruction(&Instruction::LocalSet(8));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(7)); // seen_digit
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        // If not a digit, break.
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'9' as i32));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::BrIf(1));
        // int_part = int_part * 10 + digit
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::F64Const(10.0));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::F64ConvertI32U);
        f.instruction(&Instruction::F64Add);
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Optional fractional part.
    f.instruction(&Instruction::F64Const(0.0));
    f.instruction(&Instruction::LocalSet(9)); // frac_part
    f.instruction(&Instruction::F64Const(1.0));
    f.instruction(&Instruction::LocalSet(10)); // divisor

    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem0()));
    f.instruction(&Instruction::I32Const(b'.' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Skip the '.'
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));

        // Fraction loop.
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Load8U(mem0()));
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(b'0' as i32));
            f.instruction(&Instruction::I32LtU);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(b'9' as i32));
            f.instruction(&Instruction::I32GtU);
            f.instruction(&Instruction::I32Or);
            f.instruction(&Instruction::BrIf(1));
            // frac = frac * 10 + digit, divisor *= 10
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::F64Const(10.0));
            f.instruction(&Instruction::F64Mul);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(b'0' as i32));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::F64ConvertI32U);
            f.instruction(&Instruction::F64Add);
            f.instruction(&Instruction::LocalSet(9));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::F64Const(10.0));
            f.instruction(&Instruction::F64Mul);
            f.instruction(&Instruction::LocalSet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::LocalSet(7));
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(2));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // If no digits were seen OR we didn't consume all remaining
    // characters (leftover junk like "3.14abc"), return Null.
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // result = int_part + frac_part / divisor → reuse local 8 for the
    // combined value so we can conditionally negate it without stack
    // gymnastics around the If's blocktype.
    f.instruction(&Instruction::LocalGet(8));
    f.instruction(&Instruction::LocalGet(9));
    f.instruction(&Instruction::LocalGet(10));
    f.instruction(&Instruction::F64Div);
    f.instruction(&Instruction::F64Add);
    f.instruction(&Instruction::LocalSet(8));

    // Apply sign in place.
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(8));
    f.instruction(&Instruction::F64Neg);
    f.instruction(&Instruction::LocalSet(8));
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(8));
    f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    f.instruction(&Instruction::End);
    f
}
