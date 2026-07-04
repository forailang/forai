//! Runtime helper WASM functions emitted into the module.
//!
//! These handle NaN-boxed value type dispatch (int vs float arithmetic,
//! comparisons, etc.) so that per-opcode translation stays simple.

use wasm_encoder::{Function, Instruction, MemArg, ValType};

mod abi;
mod common;
mod mem;
mod native_methods;
mod numeric;
mod objects;
mod strings_arrays;

pub use abi::*;
pub use common::*;
pub use mem::*;
use native_methods::*;
use numeric::*;
use objects::*;
use strings_arrays::*;

/// Emit all runtime helper function bodies.
/// Returns a Vec of Function in the order of RT_* constants.
pub fn emit_all(
    import_count: u32,
    import_remap: &[Option<u32>],
    ks: &KnownStrings,
    freelist_global: u32,
    live_count_global: u32,
    bucket_base: u32,
    current_task_global: Option<u32>,
    task_table: Option<(u32, i32)>,
) -> Vec<Function> {
    let base = import_count;
    vec![
        emit_is_int(),
        emit_is_float(),
        emit_as_number(base),
        emit_make_int(),
        emit_make_float(),
        emit_make_bool(),
        emit_add_with_concat(base),             // rt_add
        emit_binop_int_float(base, IntOp::Sub), // rt_sub
        emit_binop_int_float(base, IntOp::Mul), // rt_mul
        emit_div(base),                         // rt_div
        emit_idiv(base),                        // rt_idiv
        emit_mod_op(base),                      // rt_mod
        emit_pow(base),                         // rt_pow
        emit_neg(base),                         // rt_neg
        emit_cmp(base, CmpOp::Eq),              // rt_eq
        emit_cmp(base, CmpOp::Ne),              // rt_ne
        emit_cmp(base, CmpOp::Lt),              // rt_lt
        emit_cmp(base, CmpOp::Le),              // rt_le
        emit_cmp(base, CmpOp::Gt),              // rt_gt
        emit_cmp(base, CmpOp::Ge),              // rt_ge
        emit_print_val(base, import_remap),     // rt_print_val (legacy, primitives only)
        emit_itoa(),                            // rt_itoa
        emit_alloc(
            freelist_global,
            live_count_global,
            bucket_base,
            import_remap,
        ), // rt_alloc
        emit_make_obj(),                        // rt_make_obj
        emit_obj_addr(),                        // rt_obj_addr
        emit_is_obj(),                          // rt_is_obj
        // Phase 2.2: WASM-native runtime functions
        emit_str_eq(),                             // rt_str_eq
        emit_str_cmp(),                            // rt_str_cmp
        emit_alloc_string(base),                   // rt_alloc_string
        emit_concat_fn(base),                      // rt_concat
        emit_get_index(base),                      // rt_get_index
        emit_get_field(base, ks),                  // rt_get_field
        emit_set_field(base, import_remap),        // rt_set_field
        emit_print_val_new(base, import_remap),    // rt_print_val_new
        emit_value_to_str(base, ks, import_remap), // rt_value_to_str
        emit_import_module(base),                  // rt_import_module
        emit_call_native(base, import_remap),      // rt_call_native
        emit_parse_int(base),                      // rt_parse_int
        emit_parse_float(base),                    // rt_parse_float
        emit_free(
            freelist_global,
            live_count_global,
            bucket_base,
            import_remap,
        ), // rt_free
        emit_copy_deep(base),                      // rt_copy_deep
        emit_retain(base, bucket_base, import_remap), // rt_retain
        emit_release(base, bucket_base, import_remap), // rt_release
        emit_live_objects(live_count_global),      // rt_live_objects
        emit_concat_move(base),                    // rt_concat_move
        emit_current_task(current_task_global),    // rt_current_task
        emit_task_waiter(task_table),              // rt_task_waiter
        emit_task_ctx(current_task_global.zip(task_table.map(|(tb, _)| tb))), // rt_task_ctx
        emit_set_task_ctx(current_task_global.zip(task_table.map(|(tb, _)| tb))), // rt_set_task_ctx
    ]
}
