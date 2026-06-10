use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection, Function,
    FunctionSection, GlobalSection, GlobalType, ImportSection, Instruction, MemorySection,
    MemoryType, Module as EncModule, TypeSection, ValType,
};

use std::collections::HashMap;

use fai_compiler::ast::{Expression, FunctionDeclaration, Program, Statement};

#[cfg(test)]
use crate::async_emit_spec::AsyncHandlerSpec;
use crate::async_emit_spec::{
    boxed_obj_ptr, collect_unique_error_messages, frame_local_global, AsyncChildTaskSpec,
    AsyncEmitterSpec, AsyncRootTaskSpec, FrameLocal, IntBinaryOp, IntExpr, ResultExpr, StringExpr,
    ThrowValue,
};

const IMPORT_NOW_MS: u32 = 0;
const IMPORT_HOST_SET_TIMER: u32 = 1;
const FUNC_START_ASYNC: u32 = 2;
const FUNC_POLL: u32 = 3;
const FUNC_RESUME_TASK: u32 = 4;
const FUNC_TASK_RESULT: u32 = 5;
const FUNC_START_SYNC: u32 = 6;
/// `__fai_str_eq(a: i64, b: i64) -> i32` — boxed string equality.
/// Returns 1 if the strings have the same length and byte
/// content, 0 otherwise. The narrow path uses this for the
/// catch-body `e.message == 'foo'` form without pulling in the
/// full runtime.
const FUNC_STR_EQ: u32 = 7;

const GLOBAL_RESULT: u32 = 2;

const STATUS_PENDING: i32 = 1;
const STATUS_COMPLETE: i32 = 2;
const STATUS_FAILED: i32 = 3;

/// Data-section base offset. The narrow module's string constants,
/// Error dicts, and per-program string literals all live in the
/// data section starting at this address. Chosen to be well above
/// any zero-page reserved region.
const STRING_DATA_BASE: u32 = 1024;

/// Address of the baked "message" key string (the dict field name)
/// in the data section. Shared by every Error dict in the module.
/// The string layout is `[tag=OBJ_TAG_STRING][len=7]['message']`,
/// 15 bytes rounded to 16.
const KEY_STRING_ADDR: u32 = STRING_DATA_BASE;

/// Look up the data-section address of a string literal. The map
/// is populated by `build_string_data_section` at module build
/// time and read by the emit helpers. Thread-local so multiple
/// compilations don't race.
fn string_literal_addr(s: &str) -> u32 {
    STRING_ADDR_MAP
        .with(|m| m.borrow().get(s).copied())
        .unwrap_or(0)
}

/// Look up the data-section address of the Error dict for a given
/// message string. Each unique `Error('msg')` in the program gets
/// its own baked dict; this map tells the throw site which one.
fn dict_addr_for_message(message: &str) -> u32 {
    DICT_ADDR_MAP
        .with(|m| m.borrow().get(message).copied())
        .unwrap_or(0)
}

fn current_heap_global() -> u32 {
    HEAP_GLOBAL.with(|g| *g.borrow())
}

/// `MemArg` for an 8-byte aligned load at offset 0 (used by
/// `i64.load` for the runtime `e.message` field read).
fn mem0_i64() -> wasm_encoder::MemArg {
    wasm_encoder::MemArg {
        offset: 0,
        align: 3,
        memory_index: 0,
    }
}

fn mem0_i32() -> wasm_encoder::MemArg {
    wasm_encoder::MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }
}

fn mem4_i32() -> wasm_encoder::MemArg {
    wasm_encoder::MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }
}

thread_local! {
    static STRING_ADDR_MAP: std::cell::RefCell<HashMap<String, u32>> =
        std::cell::RefCell::new(HashMap::new());
    static DICT_ADDR_MAP: std::cell::RefCell<HashMap<String, u32>> =
        std::cell::RefCell::new(HashMap::new());
    static HEAP_GLOBAL: std::cell::RefCell<u32> =
        std::cell::RefCell::new(0);
}

/// Build the data section for the narrow module and populate the
/// `STRING_ADDR_MAP`. The layout is:
///   offset 1024:  "message" key string (4-byte len + bytes + pad)
///   offset 1036:  value string (4-byte len + bytes + pad)
///   offset 1044:  Error dict (24 bytes; the boxed key/value
///                 pointers are baked in as 8-byte little-endian
///                 i64 values)
///   after that:   per-program string literals, each laid out the
///                 same way as the value string.
fn build_string_data_section(spec: &AsyncEmitterSpec, error_messages: &[String]) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut string_addrs: HashMap<String, u32> = HashMap::new();

    // 1. "message" key string.
    // Layout matches the runtime: `[tag=OBJ_TAG_STRING][len][bytes]`.
    // The host reads `tag` first to classify the boxed pointer.
    let key = b"message";
    bytes.extend_from_slice(&crate::runtime::OBJ_TAG_STRING.to_le_bytes());
    bytes.extend_from_slice(&(key.len() as i32).to_le_bytes());
    bytes.extend_from_slice(key);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }

    // 2. Message strings, one per unique `Error('msg')` literal.
    //    When the program has no Error throws, the loop is empty
    //    and we skip straight to the per-program literals. The
    //    order matches `error_messages` so the throw site can
    //    resolve its dict by message string.
    let mut msg_addrs: Vec<u32> = Vec::new();
    for msg in error_messages {
        let addr = STRING_DATA_BASE + bytes.len() as u32;
        bytes.extend_from_slice(&crate::runtime::OBJ_TAG_STRING.to_le_bytes());
        bytes.extend_from_slice(&(msg.len() as i32).to_le_bytes());
        bytes.extend_from_slice(msg.as_bytes());
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        msg_addrs.push(addr);
    }

    // 3. Error dicts, one per unique message. Each dict's value
    //    field is the boxed pointer for that message's string.
    let key_boxed = boxed_obj_ptr(KEY_STRING_ADDR);
    let mut dict_addrs: Vec<u32> = Vec::new();
    for (i, _msg) in error_messages.iter().enumerate() {
        let value_boxed = boxed_obj_ptr(msg_addrs[i]);
        let dict_addr = STRING_DATA_BASE + bytes.len() as u32;
        bytes.extend_from_slice(&crate::runtime::OBJ_TAG_DICT.to_le_bytes());
        bytes.extend_from_slice(&1_i32.to_le_bytes());
        bytes.extend_from_slice(&key_boxed.to_le_bytes());
        bytes.extend_from_slice(&value_boxed.to_le_bytes());
        dict_addrs.push(dict_addr);
    }

    // 4. Per-program string literals (other than error messages and
    //    the "message" key). Each is a separate entry in the data
    //    section; the boxed pointer is its offset.
    for local in &spec.frame.locals {
        collect_string_literal(&local.value, &mut bytes, &mut string_addrs);
    }
    for local in &spec.frame.post_wait_locals {
        collect_string_literal(&local.value, &mut bytes, &mut string_addrs);
    }
    if let Some(catch_result) = &spec.handlers.catch_result {
        collect_string_literal(catch_result, &mut bytes, &mut string_addrs);
    }
    if let Some(finally_expr) = &spec.handlers.finally_expr {
        collect_string_literal(finally_expr, &mut bytes, &mut string_addrs);
    }
    collect_string_literal(&spec.root.result, &mut bytes, &mut string_addrs);

    // Populate the per-message dict address map for the throw
    // sites, and register the message strings themselves with
    // the string-literal map so a `catch e { e.message == 'msg' }`
    // condition can resolve the right-hand side as a baked
    // string constant rather than a separate literal.
    let mut dict_map: HashMap<String, u32> = HashMap::new();
    for (i, msg) in error_messages.iter().enumerate() {
        dict_map.insert(msg.clone(), dict_addrs[i]);
        string_addrs.insert(msg.clone(), msg_addrs[i]);
    }

    STRING_ADDR_MAP.with(|m| *m.borrow_mut() = string_addrs);
    DICT_ADDR_MAP.with(|m| *m.borrow_mut() = dict_map);
    bytes
}

fn collect_string_literal(
    expr: &ResultExpr,
    bytes: &mut Vec<u8>,
    addrs: &mut HashMap<String, u32>,
) {
    match expr {
        ResultExpr::String(StringExpr::Literal(s)) => {
            if !addrs.contains_key(s) {
                let addr = STRING_DATA_BASE + bytes.len() as u32;
                // Layout: [tag=OBJ_TAG_STRING][len][bytes] — matches the
                // runtime's string layout. The host reads `tag` first
                // when classifying a boxed pointer.
                bytes.extend_from_slice(&crate::runtime::OBJ_TAG_STRING.to_le_bytes());
                bytes.extend_from_slice(&(s.len() as i32).to_le_bytes());
                bytes.extend_from_slice(s.as_bytes());
                while bytes.len() % 4 != 0 {
                    bytes.push(0);
                }
                addrs.insert(s.clone(), addr);
            }
        }
        ResultExpr::IfElse {
            then_branch,
            else_branch,
            ..
        } => {
            collect_string_literal(then_branch, bytes, addrs);
            collect_string_literal(else_branch, bytes, addrs);
        }
        ResultExpr::Tuple(items) => {
            for item in items {
                collect_string_literal(item, bytes, addrs);
            }
        }
        ResultExpr::Int(_) | ResultExpr::String(StringExpr::ErrorMessage) => {}
    }
}

pub fn try_codegen_minimal_wait_main(ast: &Program) -> Option<Vec<u8>> {
    let error_messages = collect_unique_error_messages(ast);
    let first_message = error_messages.first().cloned();

    let spec = lower_async_main(ast, first_message.as_deref())?;
    Some(emit_minimal_wait_module(spec, error_messages))
}

fn lower_async_main(ast: &Program, error_message: Option<&str>) -> Option<AsyncEmitterSpec> {
    let functions = function_map(ast);
    let main = *functions.get("main")?;
    lower_main_body(&functions, &main.body, error_message)
}

fn function_map(ast: &Program) -> HashMap<&str, &FunctionDeclaration> {
    let mut functions = HashMap::new();
    for stmt in &ast.statements {
        if let Statement::FunctionDeclaration(fd) = stmt {
            functions.insert(fd.name.as_str(), fd);
        }
    }
    functions
}

fn lower_main_body(
    functions: &HashMap<&str, &FunctionDeclaration>,
    body: &[Statement],
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    if let [Statement::TryStatement(try_stmt)] = body {
        let spec = lower_main_body(functions, &try_stmt.try_body, error_message)?;
        return apply_minimal_catch(
            spec,
            &try_stmt.catch_name,
            &try_stmt.catch_body,
            try_stmt.finally_body.as_deref(),
            error_message,
        );
    }

    let mut local_indices = HashMap::new();
    let mut locals = Vec::new();
    lower_linear_main_body(
        functions,
        body,
        &mut local_indices,
        &mut locals,
        error_message,
    )
}

fn lower_linear_main_body(
    functions: &HashMap<&str, &FunctionDeclaration>,
    body: &[Statement],
    local_indices: &mut HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let (terminal, prefix) = body.split_last()?;
    let mut post_wait_locals = Vec::new();
    let mut total_delay_ms = 0.0_f64;
    let mut seen_suspend = false;
    let mut root_error = None;

    for (idx, stmt) in prefix.iter().enumerate() {
        if let Statement::LetStatement(ls) = stmt {
            if is_all_let(stmt) {
                if seen_suspend {
                    return None;
                }
                return lower_all_suspension(
                    functions,
                    ls,
                    &body[idx + 1..],
                    local_indices,
                    locals,
                    error_message,
                );
            }
            if is_single_call_let(stmt) {
                let [binding] = ls.bindings.as_slice() else {
                    return None;
                };
                let Expression::CallExpression(call) = &ls.value else {
                    return None;
                };
                let callee_name = call_callee_name(&call.callee)?;
                let phase = if seen_suspend {
                    FramePhase::Post
                } else {
                    FramePhase::Pre
                };
                let (delay_ms, child_result_expr, child_error) = lower_async_call_result(
                    functions,
                    callee_name.as_str(),
                    &call.args,
                    local_indices,
                    locals,
                    &mut post_wait_locals,
                    phase,
                    error_message,
                )?;
                total_delay_ms += delay_ms;
                seen_suspend = true;
                root_error = root_error.or(child_error);
                append_frame_local(
                    binding.name.clone(),
                    child_result_expr,
                    FramePhase::Post,
                    local_indices,
                    locals,
                    &mut post_wait_locals,
                );
                continue;
            }
            let phase = if seen_suspend {
                FramePhase::Post
            } else {
                FramePhase::Pre
            };
            append_one_let_at_phase(
                ls,
                phase,
                local_indices,
                locals,
                &mut post_wait_locals,
                None,
            )?;
            continue;
        }

        if let Statement::ExpressionStatement(es) = stmt {
            if let Some(delay_ms) = wait_call_delay_ms(&es.expression) {
                total_delay_ms += delay_ms;
                seen_suspend = true;
                continue;
            }
        }

        if let Statement::NowaitStatement(nowait) = stmt {
            if seen_suspend {
                return None;
            }
            return lower_nowait_suspension(
                functions,
                nowait,
                &body[idx + 1..],
                local_indices,
                locals,
                error_message,
            );
        }

        return None;
    }

    if !seen_suspend {
        return None;
    }
    let (result, terminal_error) =
        terminal_result_or_throw(terminal, local_indices, None, error_message)?;
    Some(AsyncEmitterSpec::new(
        std::mem::take(locals),
        post_wait_locals,
        AsyncRootTaskSpec {
            delay_ms: total_delay_ms,
            complete_immediately: false,
            result,
            error: root_error.or(terminal_error),
            local_init_start: 0,
            local_init_count: 0,
        },
        Vec::new(),
    ))
}

#[derive(Clone, Copy)]
enum FramePhase {
    Pre,
    Post,
}

fn append_frame_local(
    name: String,
    value: ResultExpr,
    phase: FramePhase,
    local_indices: &mut HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
    post_wait_locals: &mut Vec<FrameLocal>,
) -> usize {
    let idx = match phase {
        FramePhase::Pre => {
            let idx = locals.len();
            locals.push(FrameLocal {
                name: name.clone(),
                value,
            });
            idx
        }
        FramePhase::Post => {
            let idx = locals.len() + post_wait_locals.len();
            post_wait_locals.push(FrameLocal {
                name: name.clone(),
                value,
            });
            idx
        }
    };
    local_indices.insert(name, idx);
    idx
}

fn append_one_let_at_phase(
    bind: &fai_compiler::ast::LetStatement,
    phase: FramePhase,
    local_indices: &mut HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
    post_wait_locals: &mut Vec<FrameLocal>,
    catch_binding: Option<&str>,
) -> Option<()> {
    let [binding] = bind.bindings.as_slice() else {
        return None;
    };
    let value = result_expr(&bind.value, local_indices, catch_binding)?;
    append_frame_local(
        binding.name.clone(),
        value,
        phase,
        local_indices,
        locals,
        post_wait_locals,
    );
    Some(())
}

fn lower_all_suspension(
    functions: &HashMap<&str, &FunctionDeclaration>,
    bind: &fai_compiler::ast::LetStatement,
    rest: &[Statement],
    local_indices: &mut HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let Expression::CallExpression(all_call) = &bind.value else {
        return None;
    };
    let tuple_binding = bind.bindings.len() == 1;
    if (!tuple_binding && bind.bindings.len() != all_call.args.len()) || all_call.args.is_empty() {
        return None;
    }

    let mut post_wait_locals = Vec::new();
    let mut post_wait_creation_local_lens = Vec::new();
    let mut lowered_tasks = Vec::new();
    let mut result_bindings = Vec::new();
    let mut tuple_items = Vec::new();
    let mut tuple_post_idx = None;
    let mut delay_ms = 0.0_f64;
    for (idx, arg) in all_call.args.iter().enumerate() {
        let binding_name = if tuple_binding {
            format!("{}.{}", bind.bindings[0].name, idx)
        } else {
            bind.bindings[idx].name.clone()
        };
        let task = lower_all_child_task(
            functions,
            &arg.value,
            binding_name,
            local_indices,
            locals,
            &mut post_wait_locals,
            &mut post_wait_creation_local_lens,
            error_message,
        )?;

        delay_ms = delay_ms.max(task.delay_ms);
        if tuple_binding {
            tuple_items.push(ResultExpr::Int(IntExpr::Local(task.result_post_idx)));
        } else {
            result_bindings.push((bind.bindings[idx].name.clone(), task.result_post_idx));
        }
        lowered_tasks.push(task);
    }

    if tuple_binding {
        let post_idx = post_wait_locals.len();
        result_bindings.push((bind.bindings[0].name.clone(), post_idx));
        tuple_post_idx = Some(post_idx);
        post_wait_locals.push(FrameLocal {
            name: bind.bindings[0].name.clone(),
            value: ResultExpr::Tuple(tuple_items),
        });
        post_wait_creation_local_lens.push(locals.len());
    }

    let final_local_len = locals.len();
    for (local, creation_local_len) in post_wait_locals
        .iter_mut()
        .zip(post_wait_creation_local_lens.iter().copied())
    {
        shift_result_expr_post_indices(&mut local.value, creation_local_len, final_local_len);
    }
    let children = lowered_tasks
        .into_iter()
        .map(|task| AsyncChildTaskSpec {
            delay_ms: task.delay_ms,
            error: task.error,
            result_local_idx: final_local_len + task.result_post_idx,
            local_init_start: final_local_len + task.local_init_start,
            local_init_count: task.local_init_count,
        })
        .collect();
    for (name, post_idx) in result_bindings {
        local_indices.insert(name, final_local_len + post_idx);
    }

    let (result, terminal_error, root_local_init_start, root_local_init_count) =
        terminal_body_result(
            rest,
            local_indices,
            final_local_len,
            &mut post_wait_locals,
            None,
            error_message,
        )?;
    if terminal_error.is_some() {
        return None;
    }
    let (root_local_init_start, root_local_init_count) =
        if let Some(tuple_post_idx) = tuple_post_idx {
            (final_local_len + tuple_post_idx, root_local_init_count + 1)
        } else {
            (root_local_init_start, root_local_init_count)
        };

    Some(AsyncEmitterSpec::new(
        std::mem::take(locals),
        post_wait_locals,
        AsyncRootTaskSpec {
            delay_ms,
            complete_immediately: false,
            result,
            error: None,
            local_init_start: root_local_init_start,
            local_init_count: root_local_init_count,
        },
        children,
    ))
}

struct LoweredAllChildTask {
    delay_ms: f64,
    error: Option<ThrowValue>,
    result_post_idx: usize,
    local_init_start: usize,
    local_init_count: usize,
}

fn lower_all_child_task(
    functions: &HashMap<&str, &FunctionDeclaration>,
    expr: &Expression,
    result_name: String,
    local_indices: &HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
    post_wait_locals: &mut Vec<FrameLocal>,
    post_wait_creation_local_lens: &mut Vec<usize>,
    error_message: Option<&str>,
) -> Option<LoweredAllChildTask> {
    let Expression::CallExpression(child_call) = expr else {
        return None;
    };
    let callee_name = call_callee_name(&child_call.callee)?;
    let child_post_start = post_wait_locals.len();
    let (child_delay, child_result, child_error) = lower_async_call_result(
        functions,
        callee_name.as_str(),
        &child_call.args,
        local_indices,
        locals,
        post_wait_locals,
        FramePhase::Pre,
        error_message,
    )?;

    let child_creation_local_len = locals.len();
    for _ in child_post_start..post_wait_locals.len() {
        post_wait_creation_local_lens.push(child_creation_local_len);
    }
    let result_post_idx = post_wait_locals.len();
    post_wait_locals.push(FrameLocal {
        name: result_name,
        value: child_result,
    });
    post_wait_creation_local_lens.push(child_creation_local_len);
    Some(LoweredAllChildTask {
        delay_ms: child_delay,
        error: child_error,
        result_post_idx,
        local_init_start: child_post_start,
        local_init_count: post_wait_locals.len() - child_post_start,
    })
}

fn lower_nowait_suspension(
    functions: &HashMap<&str, &FunctionDeclaration>,
    nowait: &fai_compiler::ast::NowaitStatement,
    rest: &[Statement],
    local_indices: &mut HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let Expression::CallExpression(child_call) = &nowait.expression else {
        return None;
    };
    let callee_name = call_callee_name(&child_call.callee)?;
    let child = *functions.get(callee_name.as_str())?;
    if child.name == "main" || child.params.len() != child_call.args.len() {
        return None;
    }

    let mut child_indices = HashMap::new();
    for (param, arg) in child.params.iter().zip(child_call.args.iter()) {
        let value = result_expr(&arg.value, local_indices, None)?;
        let idx = locals.len();
        child_indices.insert(param.name.clone(), idx);
        locals.push(FrameLocal {
            name: format!("{}.{}", child.name, param.name),
            value,
        });
    }

    let (child_delay, child_result, child_error) = lower_async_function_result(
        functions,
        child,
        &mut child_indices,
        locals,
        &mut Vec::new(),
        FramePhase::Pre,
        error_message,
    )?;

    let mut post_wait_locals = Vec::new();
    let (result, terminal_error, _, terminal_local_count) = terminal_body_result(
        rest,
        local_indices,
        locals.len(),
        &mut post_wait_locals,
        None,
        error_message,
    )?;
    if terminal_error.is_some() || terminal_local_count > 0 {
        return None;
    }
    let child_result_idx = locals.len();

    Some(AsyncEmitterSpec::new(
        std::mem::take(locals),
        vec![FrameLocal {
            name: format!("<nowait.{}>", child.name),
            value: child_result,
        }],
        AsyncRootTaskSpec {
            delay_ms: 0.0,
            complete_immediately: true,
            result,
            error: None,
            local_init_start: 0,
            local_init_count: 0,
        },
        vec![AsyncChildTaskSpec {
            delay_ms: child_delay,
            error: child_error,
            result_local_idx: child_result_idx,
            local_init_start: child_result_idx,
            local_init_count: 1,
        }],
    ))
}

fn lower_async_call_result(
    functions: &HashMap<&str, &FunctionDeclaration>,
    callee_name: &str,
    call_args: &[fai_compiler::ast::CallArgument],
    caller_indices: &HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
    post_wait_locals: &mut Vec<FrameLocal>,
    phase: FramePhase,
    error_message: Option<&str>,
) -> Option<(f64, ResultExpr, Option<ThrowValue>)> {
    let child = *functions.get(callee_name)?;
    if child.name == "main" || child.params.len() != call_args.len() {
        return None;
    }

    let mut child_indices = HashMap::new();
    for (param, arg) in child.params.iter().zip(call_args.iter()) {
        let value = result_expr(&arg.value, caller_indices, None)?;
        append_frame_local(
            param.name.clone(),
            value,
            phase,
            &mut child_indices,
            locals,
            post_wait_locals,
        );
        if let Some(local) = match phase {
            FramePhase::Pre => locals.last_mut(),
            FramePhase::Post => post_wait_locals.last_mut(),
        } {
            local.name = format!("{}.{}", child.name, param.name);
        }
    }

    lower_async_function_result(
        functions,
        child,
        &mut child_indices,
        locals,
        post_wait_locals,
        phase,
        error_message,
    )
}

fn call_callee_name(expr: &Expression) -> Option<String> {
    match expr {
        Expression::IdentifierExpression(callee) => Some(callee.name.clone()),
        Expression::MemberExpression(member) => {
            let mut path = callee_path_segments(&member.object)?;
            path.push(member.property.clone());
            Some(path.join("."))
        }
        _ => None,
    }
}

fn callee_path_segments(expr: &Expression) -> Option<Vec<String>> {
    match expr {
        Expression::IdentifierExpression(id) => Some(vec![id.name.clone()]),
        Expression::MemberExpression(member) => {
            let mut path = callee_path_segments(&member.object)?;
            path.push(member.property.clone());
            Some(path)
        }
        _ => None,
    }
}

fn lower_async_function_result(
    functions: &HashMap<&str, &FunctionDeclaration>,
    function: &FunctionDeclaration,
    local_indices: &mut HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
    post_wait_locals: &mut Vec<FrameLocal>,
    initial_phase: FramePhase,
    error_message: Option<&str>,
) -> Option<(f64, ResultExpr, Option<ThrowValue>)> {
    let (terminal, prefix) = function.body.split_last()?;
    let mut total_delay_ms = 0.0_f64;
    let mut seen_suspend = false;
    let mut root_error = None;

    for stmt in prefix {
        if let Statement::LetStatement(ls) = stmt {
            if is_single_call_let(stmt) && !is_all_let(stmt) {
                let [binding] = ls.bindings.as_slice() else {
                    return None;
                };
                let Expression::CallExpression(call) = &ls.value else {
                    return None;
                };
                let callee_name = call_callee_name(&call.callee)?;
                let (delay_ms, nested_result, nested_error) = lower_async_call_result(
                    functions,
                    callee_name.as_str(),
                    &call.args,
                    local_indices,
                    locals,
                    post_wait_locals,
                    if seen_suspend {
                        FramePhase::Post
                    } else {
                        initial_phase
                    },
                    error_message,
                )?;
                total_delay_ms += delay_ms;
                seen_suspend = true;
                root_error = root_error.or(nested_error);
                append_frame_local(
                    binding.name.clone(),
                    nested_result,
                    FramePhase::Post,
                    local_indices,
                    locals,
                    post_wait_locals,
                );
                if let Some(local) = post_wait_locals.last_mut() {
                    local.name = format!("{}.{}", function.name, binding.name);
                }
                continue;
            }
            let phase = if seen_suspend {
                FramePhase::Post
            } else {
                initial_phase
            };
            append_one_let_at_phase(ls, phase, local_indices, locals, post_wait_locals, None)?;
            continue;
        }

        if let Statement::ExpressionStatement(es) = stmt {
            if let Some(delay_ms) = wait_call_delay_ms(&es.expression) {
                total_delay_ms += delay_ms;
                seen_suspend = true;
                continue;
            }
        }

        return None;
    }

    if !seen_suspend {
        return None;
    }
    let (result, terminal_error) =
        terminal_result_or_throw(terminal, local_indices, None, error_message)?;
    Some((total_delay_ms, result, root_error.or(terminal_error)))
}

#[cfg(test)]
fn extract_minimal_wait_main(
    ast: &Program,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let main = ast.statements.iter().find_map(|stmt| match stmt {
        Statement::FunctionDeclaration(fd) if fd.name == "main" => Some(fd),
        _ => None,
    })?;
    extract_minimal_wait_body(main, error_message)
}

#[cfg(test)]
fn extract_minimal_try_main(
    ast: &Program,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let main = ast.statements.iter().find_map(|stmt| match stmt {
        Statement::FunctionDeclaration(fd) if fd.name == "main" => Some(fd),
        _ => None,
    })?;
    let [Statement::TryStatement(try_stmt)] = main.body.as_slice() else {
        return None;
    };

    let mut try_ast = ast.clone();
    for stmt in &mut try_ast.statements {
        if let Statement::FunctionDeclaration(fd) = stmt {
            if fd.name == "main" {
                fd.body = try_stmt.try_body.clone();
            }
        }
    }

    let spec = extract_minimal_wait_main(&try_ast, error_message)
        .or_else(|| extract_minimal_auto_await_main(&try_ast, error_message))
        .or_else(|| extract_minimal_all_main(&try_ast, error_message))?;
    apply_minimal_catch(
        spec,
        &try_stmt.catch_name,
        &try_stmt.catch_body,
        try_stmt.finally_body.as_deref(),
        error_message,
    )
}

#[cfg(test)]
fn extract_minimal_wait_body(
    main: &FunctionDeclaration,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let wait_idx = main.body.iter().position(|stmt| match stmt {
        Statement::ExpressionStatement(es) => wait_call_delay_ms(&es.expression).is_some(),
        _ => false,
    })?;
    if wait_idx + 2 != main.body.len() {
        return None;
    }

    let mut local_indices = HashMap::new();
    let mut locals = Vec::new();
    append_pre_wait_lets(&main.body[..wait_idx], &mut local_indices, &mut locals)?;

    let delay_ms = match &main.body[wait_idx] {
        Statement::ExpressionStatement(es) => wait_call_delay_ms(&es.expression)?,
        _ => return None,
    };
    let (result, root_error) = terminal_result_or_throw(
        &main.body[wait_idx + 1],
        &local_indices,
        None,
        error_message,
    )?;
    Some(AsyncEmitterSpec::new(
        locals,
        Vec::new(),
        AsyncRootTaskSpec {
            delay_ms,
            complete_immediately: false,
            result,
            error: root_error,
            local_init_start: 0,
            local_init_count: 0,
        },
        Vec::new(),
    ))
}

#[cfg(test)]
fn extract_minimal_auto_await_main(
    ast: &Program,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let mut functions = HashMap::new();
    for stmt in &ast.statements {
        if let Statement::FunctionDeclaration(fd) = stmt {
            functions.insert(fd.name.as_str(), fd);
        }
    }
    let main = *functions.get("main")?;
    if main.body.len() < 2 {
        return None;
    }
    let call_idx = main.body[..main.body.len() - 1]
        .iter()
        .position(is_single_call_let)?;
    if call_idx + 2 != main.body.len() {
        return None;
    }
    let bind_stmt = &main.body[call_idx];
    let result_stmt = &main.body[call_idx + 1];

    let mut main_indices = HashMap::new();
    let mut locals = Vec::new();
    append_pre_wait_lets(&main.body[..call_idx], &mut main_indices, &mut locals)?;

    let Statement::LetStatement(bind) = bind_stmt else {
        return None;
    };
    let [binding] = bind.bindings.as_slice() else {
        return None;
    };
    let Expression::CallExpression(call) = &bind.value else {
        return None;
    };
    let Expression::IdentifierExpression(callee) = &*call.callee else {
        return None;
    };
    let mut post_wait_locals = Vec::new();
    let (delay_ms, child_result_expr, child_error) = extract_async_call_result(
        &functions,
        callee.name.as_str(),
        &call.args,
        &main_indices,
        &mut locals,
        &mut post_wait_locals,
        error_message,
    )?;

    let child_result_idx = locals.len() + post_wait_locals.len();
    main_indices.insert(binding.name.clone(), child_result_idx);
    post_wait_locals.push(FrameLocal {
        name: binding.name.clone(),
        value: child_result_expr,
    });
    let result = match result_stmt {
        Statement::ExpressionStatement(es) => result_expr(&es.expression, &main_indices, None)?,
        Statement::ReturnStatement(rs) => result_expr(rs.value.as_ref()?, &main_indices, None)?,
        _ => return None,
    };

    Some(AsyncEmitterSpec::new(
        locals,
        post_wait_locals,
        AsyncRootTaskSpec {
            delay_ms,
            complete_immediately: false,
            result,
            error: child_error,
            local_init_start: 0,
            local_init_count: 0,
        },
        Vec::new(),
    ))
}

#[cfg(test)]
fn extract_async_call_result(
    functions: &HashMap<&str, &FunctionDeclaration>,
    callee_name: &str,
    call_args: &[fai_compiler::ast::CallArgument],
    caller_indices: &HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
    post_wait_locals: &mut Vec<FrameLocal>,
    error_message: Option<&str>,
) -> Option<(f64, ResultExpr, Option<ThrowValue>)> {
    let child = *functions.get(callee_name)?;
    if child.name == "main" || child.params.len() != call_args.len() {
        return None;
    }

    let mut child_indices = HashMap::new();
    for (param, arg) in child.params.iter().zip(call_args.iter()) {
        let value = result_expr(&arg.value, caller_indices, None)?;
        let idx = locals.len();
        child_indices.insert(param.name.clone(), idx);
        locals.push(FrameLocal {
            name: format!("{}.{}", child.name, param.name),
            value,
        });
    }

    if let Some(wait_idx) = child.body.iter().position(|stmt| match stmt {
        Statement::ExpressionStatement(es) => wait_call_delay_ms(&es.expression).is_some(),
        _ => false,
    }) {
        if wait_idx + 2 != child.body.len() {
            return None;
        }
        append_pre_wait_lets(&child.body[..wait_idx], &mut child_indices, locals)?;
        let delay_ms = match &child.body[wait_idx] {
            Statement::ExpressionStatement(es) => wait_call_delay_ms(&es.expression)?,
            _ => return None,
        };
        let (child_result, child_error) = terminal_result_or_throw(
            &child.body[wait_idx + 1],
            &child_indices,
            None,
            error_message,
        )?;
        return Some((delay_ms, child_result, child_error));
    }

    let call_idx = child.body[..child.body.len().saturating_sub(1)]
        .iter()
        .position(is_single_call_let)?;
    if call_idx + 2 != child.body.len() {
        return None;
    }
    append_pre_wait_lets(&child.body[..call_idx], &mut child_indices, locals)?;

    let Statement::LetStatement(bind) = &child.body[call_idx] else {
        return None;
    };
    let [binding] = bind.bindings.as_slice() else {
        return None;
    };
    let Expression::CallExpression(call) = &bind.value else {
        return None;
    };
    let Expression::IdentifierExpression(nested_callee) = &*call.callee else {
        return None;
    };
    let (delay_ms, nested_result, nested_error) = extract_async_call_result(
        functions,
        nested_callee.name.as_str(),
        &call.args,
        &child_indices,
        locals,
        post_wait_locals,
        error_message,
    )?;

    let binding_idx = locals.len() + post_wait_locals.len();
    child_indices.insert(binding.name.clone(), binding_idx);
    post_wait_locals.push(FrameLocal {
        name: format!("{}.{}", child.name, binding.name),
        value: nested_result,
    });

    let terminal = &child.body[call_idx + 1];
    let (result, terminal_error) =
        terminal_result_or_throw(terminal, &child_indices, None, error_message)?;
    Some((delay_ms, result, nested_error.or(terminal_error)))
}

#[cfg(test)]
fn extract_minimal_all_main(
    ast: &Program,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let mut functions = HashMap::new();
    for stmt in &ast.statements {
        if let Statement::FunctionDeclaration(fd) = stmt {
            functions.insert(fd.name.as_str(), fd);
        }
    }
    let main = *functions.get("main")?;
    if main.body.len() < 2 {
        return None;
    }
    let all_idx = main.body[..main.body.len() - 1]
        .iter()
        .position(is_all_let)?;
    if all_idx + 2 != main.body.len() {
        return None;
    }

    let mut main_indices = HashMap::new();
    let mut locals = Vec::new();
    append_pre_wait_lets(&main.body[..all_idx], &mut main_indices, &mut locals)?;

    let Statement::LetStatement(bind) = &main.body[all_idx] else {
        return None;
    };
    let Expression::CallExpression(all_call) = &bind.value else {
        return None;
    };
    if bind.bindings.len() != all_call.args.len() || all_call.args.is_empty() {
        return None;
    }

    let mut post_wait_locals = Vec::new();
    let mut post_wait_creation_local_lens = Vec::new();
    let mut child_delays_ms = Vec::new();
    let mut child_errors = Vec::new();
    let mut child_result_post_indices = Vec::new();
    let mut child_post_init_ranges = Vec::new();
    let mut result_bindings = Vec::new();
    let mut delay_ms = 0.0_f64;
    for (binding, arg) in bind.bindings.iter().zip(all_call.args.iter()) {
        let Expression::CallExpression(child_call) = &arg.value else {
            return None;
        };
        let Expression::IdentifierExpression(callee) = &*child_call.callee else {
            return None;
        };
        let child_post_start = post_wait_locals.len();
        let (child_delay, child_result, child_error) = extract_async_call_result(
            &functions,
            callee.name.as_str(),
            &child_call.args,
            &main_indices,
            &mut locals,
            &mut post_wait_locals,
            error_message,
        )?;

        delay_ms = delay_ms.max(child_delay);
        child_delays_ms.push(child_delay);
        child_errors.push(child_error);
        let child_creation_local_len = locals.len();
        for _ in child_post_start..post_wait_locals.len() {
            post_wait_creation_local_lens.push(child_creation_local_len);
        }
        let child_result_post_idx = post_wait_locals.len();
        child_result_post_indices.push(child_result_post_idx);
        result_bindings.push((binding.name.clone(), child_result_post_idx));
        post_wait_locals.push(FrameLocal {
            name: binding.name.clone(),
            value: child_result,
        });
        post_wait_creation_local_lens.push(child_creation_local_len);
        let child_post_init_count = post_wait_locals.len() - child_post_start;
        child_post_init_ranges.push((child_post_start, child_post_init_count));
    }
    let final_local_len = locals.len();
    for (local, creation_local_len) in post_wait_locals
        .iter_mut()
        .zip(post_wait_creation_local_lens.iter().copied())
    {
        shift_result_expr_post_indices(&mut local.value, creation_local_len, final_local_len);
    }
    let children = child_delays_ms
        .into_iter()
        .zip(child_errors)
        .zip(child_result_post_indices)
        .zip(child_post_init_ranges)
        .map(
            |(((delay_ms, error), result_post_idx), (local_init_start, local_init_count))| {
                AsyncChildTaskSpec {
                    delay_ms,
                    error,
                    result_local_idx: final_local_len + result_post_idx,
                    local_init_start: final_local_len + local_init_start,
                    local_init_count,
                }
            },
        )
        .collect();
    for (name, post_idx) in result_bindings {
        main_indices.insert(name, final_local_len + post_idx);
    }

    let result_stmt = &main.body[all_idx + 1];
    let result = match result_stmt {
        Statement::ExpressionStatement(es) => result_expr(&es.expression, &main_indices, None)?,
        Statement::ReturnStatement(rs) => result_expr(rs.value.as_ref()?, &main_indices, None)?,
        _ => return None,
    };

    Some(AsyncEmitterSpec::new(
        locals,
        post_wait_locals,
        AsyncRootTaskSpec {
            delay_ms,
            complete_immediately: false,
            result,
            error: None,
            local_init_start: 0,
            local_init_count: 0,
        },
        children,
    ))
}

#[cfg(test)]
fn extract_minimal_nowait_main(
    ast: &Program,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let mut functions = HashMap::new();
    for stmt in &ast.statements {
        if let Statement::FunctionDeclaration(fd) = stmt {
            functions.insert(fd.name.as_str(), fd);
        }
    }
    let main = *functions.get("main")?;
    if main.body.len() < 2 {
        return None;
    }
    let nowait_idx = main.body[..main.body.len() - 1]
        .iter()
        .position(|stmt| matches!(stmt, Statement::NowaitStatement(_)))?;
    if nowait_idx + 2 != main.body.len() {
        return None;
    }

    let mut main_indices = HashMap::new();
    let mut locals = Vec::new();
    append_pre_wait_lets(&main.body[..nowait_idx], &mut main_indices, &mut locals)?;

    let Statement::NowaitStatement(nowait) = &main.body[nowait_idx] else {
        return None;
    };
    let Expression::CallExpression(child_call) = &nowait.expression else {
        return None;
    };
    let Expression::IdentifierExpression(callee) = &*child_call.callee else {
        return None;
    };
    let child = *functions.get(callee.name.as_str())?;
    if child.name == "main" || child.params.len() != child_call.args.len() {
        return None;
    }

    let mut child_indices = HashMap::new();
    for (param, arg) in child.params.iter().zip(child_call.args.iter()) {
        let value = int_expr(&arg.value, &main_indices, None)?;
        let idx = locals.len();
        child_indices.insert(param.name.clone(), idx);
        locals.push(FrameLocal {
            name: format!("{}.{}", child.name, param.name),
            value: lift_int_to_result(value),
        });
    }

    let wait_idx = child.body.iter().position(|stmt| match stmt {
        Statement::ExpressionStatement(es) => wait_call_delay_ms(&es.expression).is_some(),
        _ => false,
    })?;
    if wait_idx + 2 != child.body.len() {
        return None;
    }
    append_pre_wait_lets(&child.body[..wait_idx], &mut child_indices, &mut locals)?;
    let child_delay = match &child.body[wait_idx] {
        Statement::ExpressionStatement(es) => wait_call_delay_ms(&es.expression)?,
        _ => return None,
    };
    let (child_result, child_throw) =
        terminal_int_or_throw(&child.body[wait_idx + 1], &child_indices)?;
    let mut child_error = child_throw.and_then(|expr| match expr {
        IntExpr::Literal(-1) => None, // sentinel: actual throw is Error('msg')
        IntExpr::Literal(v) => Some(ThrowValue::IntLiteral(v)),
        _ => None,
    });
    if child_error.is_none() {
        if error_message.is_some() {
            if let Statement::ThrowStatement(ts) = &child.body[wait_idx + 1] {
                child_error = Some(throw_value(&ts.expression, None)?);
            }
        }
    }

    let result_stmt = &main.body[nowait_idx + 1];
    let result = match result_stmt {
        Statement::ExpressionStatement(es) => result_expr(&es.expression, &main_indices, None)?,
        Statement::ReturnStatement(rs) => result_expr(rs.value.as_ref()?, &main_indices, None)?,
        _ => return None,
    };
    let child_result_idx = locals.len();

    Some(AsyncEmitterSpec::new(
        locals,
        vec![FrameLocal {
            name: format!("<nowait.{}>", child.name),
            value: lift_int_to_result(child_result),
        }],
        AsyncRootTaskSpec {
            delay_ms: 0.0,
            complete_immediately: true,
            result,
            error: None,
            local_init_start: 0,
            local_init_count: 0,
        },
        vec![AsyncChildTaskSpec {
            delay_ms: child_delay,
            error: child_error,
            result_local_idx: child_result_idx,
            local_init_start: child_result_idx,
            local_init_count: 1,
        }],
    ))
}

#[cfg(test)]
fn append_pre_wait_lets(
    stmts: &[Statement],
    local_indices: &mut HashMap<String, usize>,
    locals: &mut Vec<FrameLocal>,
) -> Option<()> {
    for stmt in stmts {
        let Statement::LetStatement(ls) = stmt else {
            return None;
        };
        let [binding] = ls.bindings.as_slice() else {
            return None;
        };
        let value = result_expr(&ls.value, local_indices, None)?;
        let idx = locals.len();
        local_indices.insert(binding.name.clone(), idx);
        locals.push(FrameLocal {
            name: binding.name.clone(),
            value,
        });
    }
    Some(())
}

fn append_post_wait_lets(
    stmts: &[Statement],
    local_indices: &mut HashMap<String, usize>,
    pre_local_count: usize,
    post_wait_locals: &mut Vec<FrameLocal>,
    catch_binding: Option<&str>,
) -> Option<()> {
    for stmt in stmts {
        let Statement::LetStatement(ls) = stmt else {
            return None;
        };
        let [binding] = ls.bindings.as_slice() else {
            return None;
        };
        let value = result_expr(&ls.value, local_indices, catch_binding)?;
        let idx = pre_local_count + post_wait_locals.len();
        local_indices.insert(binding.name.clone(), idx);
        post_wait_locals.push(FrameLocal {
            name: binding.name.clone(),
            value,
        });
    }
    Some(())
}

fn terminal_body_result(
    body: &[Statement],
    local_indices: &mut HashMap<String, usize>,
    pre_local_count: usize,
    post_wait_locals: &mut Vec<FrameLocal>,
    catch_binding: Option<&str>,
    seen_error_message: Option<&str>,
) -> Option<(ResultExpr, Option<ThrowValue>, usize, usize)> {
    let (terminal, prefix) = body.split_last()?;
    let start = pre_local_count + post_wait_locals.len();
    append_post_wait_lets(
        prefix,
        local_indices,
        pre_local_count,
        post_wait_locals,
        catch_binding,
    )?;
    let count = pre_local_count + post_wait_locals.len() - start;
    let (result, error) =
        terminal_result_or_throw(terminal, local_indices, catch_binding, seen_error_message)?;
    Some((result, error, start, count))
}

fn local_indices_for_existing_spec(spec: &AsyncEmitterSpec) -> HashMap<String, usize> {
    let mut indices = HashMap::new();
    for (idx, local) in spec.frame.locals.iter().enumerate() {
        indices.insert(local.name.clone(), idx);
    }
    for (idx, local) in spec.frame.post_wait_locals.iter().enumerate() {
        indices.insert(local.name.clone(), spec.frame.locals.len() + idx);
    }
    indices
}

fn is_single_call_let(stmt: &Statement) -> bool {
    let Statement::LetStatement(bind) = stmt else {
        return false;
    };
    if bind.bindings.len() != 1 {
        return false;
    }
    matches!(&bind.value, Expression::CallExpression(_))
}

fn is_all_let(stmt: &Statement) -> bool {
    let Statement::LetStatement(bind) = stmt else {
        return false;
    };
    let Expression::CallExpression(call) = &bind.value else {
        return false;
    };
    let Expression::IdentifierExpression(callee) = &*call.callee else {
        return false;
    };
    callee.name == "all"
}

fn wait_call_delay_ms(expr: &Expression) -> Option<f64> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::IdentifierExpression(callee) = &*call.callee else {
        return None;
    };
    if callee.name != "sleep" {
        return None;
    }
    let [arg] = call.args.as_slice() else {
        return None;
    };
    let Expression::NumberExpression(n) = &arg.value else {
        return None;
    };
    Some(n.value.max(0.0))
}

fn int_expr(
    expr: &Expression,
    locals: &HashMap<String, usize>,
    catch_binding: Option<&str>,
) -> Option<IntExpr> {
    match expr {
        Expression::NumberExpression(n) => {
            if n.is_float || n.value != (n.value as i32) as f64 {
                return None;
            }
            Some(IntExpr::Literal(n.value as i32))
        }
        Expression::IdentifierExpression(id) => locals.get(&id.name).copied().map(IntExpr::Local),
        Expression::BinaryExpression(bin) => {
            // `e.message == 'foo'` is a string equality; both sides
            // are boxed strings. We surface that as an `IntExpr::StringEq`
            // so the emitter can inline a byte compare.
            if bin.operator == "==" {
                let left = string_expr(&bin.left, catch_binding)?;
                let right = string_expr(&bin.right, catch_binding)?;
                return Some(IntExpr::StringEq {
                    left: Box::new(left),
                    right: Box::new(right),
                });
            }
            let op = match bin.operator.as_str() {
                "+" => IntBinaryOp::Add,
                "-" => IntBinaryOp::Sub,
                "*" => IntBinaryOp::Mul,
                _ => return None,
            };
            Some(IntExpr::Binary {
                op,
                left: Box::new(int_expr(&bin.left, locals, catch_binding)?),
                right: Box::new(int_expr(&bin.right, locals, catch_binding)?),
            })
        }
        Expression::IndexExpression(index) => {
            let Expression::IdentifierExpression(object) = &*index.object else {
                return None;
            };
            let Expression::NumberExpression(index_value) = &*index.index else {
                return None;
            };
            if index_value.is_float || index_value.value != (index_value.value as i32) as f64 {
                return None;
            }
            Some(IntExpr::TupleIndex {
                tuple_local_idx: *locals.get(&object.name)?,
                index: index_value.value as i32,
            })
        }
        _ => None,
    }
}

/// Build a `StringExpr` for a position expression. Used for the
/// `e.message` and string-literal cases, and for the operands of
/// string `==`. `catch_binding` is the catch name when this is
/// evaluated inside a catch body; required for the `e.message`
/// member-access form.
fn string_expr(expr: &Expression, catch_binding: Option<&str>) -> Option<StringExpr> {
    match expr {
        Expression::StringExpression(s) => Some(StringExpr::Literal(s.value.clone())),
        Expression::MemberExpression(me) => {
            // Only support `<catch_binding>.message`.
            let Expression::IdentifierExpression(obj) = &*me.object else {
                return None;
            };
            let Some(catch_name) = catch_binding else {
                return None;
            };
            if obj.name != catch_name || me.property != "message" {
                return None;
            }
            Some(StringExpr::ErrorMessage)
        }
        _ => None,
    }
}

/// Build a `ResultExpr` from a position expression. Used for
/// result slots that may be either int or string — `e.message` for
/// a catch body, for instance, returns a String.
fn result_expr(
    expr: &Expression,
    locals: &HashMap<String, usize>,
    catch_binding: Option<&str>,
) -> Option<ResultExpr> {
    match expr {
        Expression::NumberExpression(_)
        | Expression::IdentifierExpression(_)
        | Expression::IndexExpression(_)
        | Expression::BinaryExpression(_) => {
            int_expr(expr, locals, catch_binding).map(ResultExpr::Int)
        }
        Expression::StringExpression(s) => {
            Some(ResultExpr::String(StringExpr::Literal(s.value.clone())))
        }
        Expression::MemberExpression(_) => {
            // `<catch_binding>.message` — re-use `string_expr` for
            // the field-access shape.
            string_expr(expr, catch_binding).map(ResultExpr::String)
        }
        _ => None,
    }
}

/// Classify a throw-site expression into a `ThrowValue`. Accepts
/// `throw 7` (IntLiteral) and `throw Error('msg')` (ErrorDict).
/// Rejects everything else so the narrow path falls through to the
/// regular compiler. The optional parameter is kept for older
/// callers, but the emitter now bakes every unique `Error(...)`
/// message and can preserve distinct throw values.
fn throw_value(expr: &Expression, _seen_error_message: Option<&str>) -> Option<ThrowValue> {
    match expr {
        Expression::NumberExpression(n) => {
            if n.is_float || n.value != (n.value as i32) as f64 {
                return None;
            }
            Some(ThrowValue::IntLiteral(n.value as i32))
        }
        Expression::CallExpression(call) => {
            // `Error('msg')` — a single string-argument call.
            let Expression::IdentifierExpression(callee) = &*call.callee else {
                return None;
            };
            if callee.name != "Error" {
                return None;
            }
            let [arg] = call.args.as_slice() else {
                return None;
            };
            let Expression::StringExpression(s) = &arg.value else {
                return None;
            };
            Some(ThrowValue::ErrorDict(s.value.clone()))
        }
        _ => None,
    }
}

/// Classify a terminal statement as either a result expression or
/// a throw. The narrow async path supports integer arithmetic,
/// string literals, `e.message` in a catch body, plain int throw,
/// `throw Error('msg')`, and `if/else` with single-statement
/// branches as a catch-body terminal. Anything else returns `None`
/// and the program falls through to the regular direct compiler.
fn terminal_result_or_throw(
    stmt: &Statement,
    locals: &HashMap<String, usize>,
    catch_binding: Option<&str>,
    seen_error_message: Option<&str>,
) -> Option<(ResultExpr, Option<ThrowValue>)> {
    match stmt {
        Statement::ExpressionStatement(es) => {
            Some((result_expr(&es.expression, locals, catch_binding)?, None))
        }
        Statement::ReturnStatement(rs) => {
            let value = rs.value.as_ref()?;
            Some((result_expr(value, locals, catch_binding)?, None))
        }
        Statement::IfStatement(is) => {
            // The narrow path supports a single `if/else` with
            // single-statement branches as a catch-body (or
            // success-result) terminal. Multiple branches via
            // `elsif` are not in scope here.
            if is.branches.len() != 1 {
                return None;
            }
            let branch = &is.branches[0];
            let condition = int_expr(&branch.condition, locals, catch_binding)?;
            if branch.body.len() != 1 {
                return None;
            }
            let then_branch = terminal_result_or_throw(
                &branch.body[0],
                locals,
                catch_binding,
                seen_error_message,
            )?;
            if then_branch.1.is_some() {
                return None;
            }
            let else_body = is.else_branch.as_ref()?;
            if else_body.len() != 1 {
                return None;
            }
            let else_branch =
                terminal_result_or_throw(&else_body[0], locals, catch_binding, seen_error_message)?;
            if else_branch.1.is_some() {
                return None;
            }
            Some((
                ResultExpr::IfElse {
                    condition: Box::new(condition),
                    then_branch: Box::new(then_branch.0),
                    else_branch: Box::new(else_branch.0),
                },
                None,
            ))
        }
        Statement::ThrowStatement(ts) => {
            // The "result" slot of a throw is a placeholder; the
            // throw carries the meaningful value.
            Some((
                ResultExpr::Int(IntExpr::Literal(0)),
                Some(throw_value(&ts.expression, seen_error_message)?),
            ))
        }
        _ => None,
    }
}

/// Legacy child-only terminal classifier. Children in the narrow
/// path still only return ints (the `ResultExpr` widening is only
/// applied to the root result and catch body). Keeps the
/// `IntExpr`-based post_wait_local initialization intact.
#[cfg(test)]
fn terminal_int_or_throw(
    stmt: &Statement,
    locals: &HashMap<String, usize>,
) -> Option<(IntExpr, Option<IntExpr>)> {
    match stmt {
        Statement::ExpressionStatement(es) => Some((int_expr(&es.expression, locals, None)?, None)),
        Statement::ReturnStatement(rs) => Some((int_expr(rs.value.as_ref()?, locals, None)?, None)),
        Statement::ThrowStatement(ts) => {
            // Try an int throw first, then an `Error(...)` throw.
            // The legacy child classifier returns a placeholder
            // throw value; the caller (extract_minimal_auto_await_main
            // etc.) re-classifies it via `throw_value` when the
            // program has a unique error message. The throw value
            // here is a literal int if the throw was `throw 7`,
            // and the special INT_LITERAL_OF_ERROR value otherwise
            // — but the simpler form is to just stash a dummy
            // throw and let the caller recognise the call shape.
            if let Some(int_throw) = int_expr(&ts.expression, locals, None) {
                Some((IntExpr::Literal(0), Some(int_throw)))
            } else if is_error_call(&ts.expression) {
                // Signal an Error throw to the caller via a
                // dummy IntExpr::Literal(-1) — the caller will
                // re-classify via throw_value() if a unique
                // error message is in scope.
                Some((IntExpr::Literal(0), Some(IntExpr::Literal(-1))))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// True if the expression is `Error(...)` — a bare call to the
/// Error constructor. Used by the legacy child classifier to
/// distinguish `throw Error('msg')` from `throw <int>`.
#[cfg(test)]
fn is_error_call(expr: &Expression) -> bool {
    let Expression::CallExpression(call) = expr else {
        return false;
    };
    let Expression::IdentifierExpression(callee) = &*call.callee else {
        return false;
    };
    callee.name == "Error"
}

/// Lift a raw `IntExpr` to a `ResultExpr`. Used at every call site
/// that needs to populate a result slot (root, catch, finally) from
/// an expression we already know is an integer.
#[cfg(test)]
fn lift_int_to_result(expr: IntExpr) -> ResultExpr {
    ResultExpr::Int(expr)
}

fn apply_minimal_catch(
    mut spec: AsyncEmitterSpec,
    catch_name: &str,
    catch_body: &[Statement],
    finally_body: Option<&[Statement]>,
    error_message: Option<&str>,
) -> Option<AsyncEmitterSpec> {
    let catch_idx = spec.frame.locals.len() + spec.frame.post_wait_locals.len();
    let mut catch_indices = local_indices_for_existing_spec(&spec);
    catch_indices.insert(catch_name.to_string(), catch_idx);
    spec.frame.post_wait_locals.push(FrameLocal {
        name: catch_name.to_string(),
        value: ResultExpr::Int(IntExpr::Literal(0)),
    });
    // The catch body can throw a value too, but a re-throw in catch
    // is not in the narrow shape. Reject it here.
    let (catch_result, catch_error, catch_local_init_start, catch_local_init_count) =
        terminal_body_result(
            catch_body,
            &mut catch_indices,
            spec.frame.locals.len(),
            &mut spec.frame.post_wait_locals,
            Some(catch_name),
            None,
        )?;
    if catch_error.is_some() {
        return None;
    }
    spec.handlers.catch_error_local_idx = Some(catch_idx);
    spec.handlers.catch_local_init_start = catch_local_init_start;
    spec.handlers.catch_local_init_count = catch_local_init_count;
    spec.handlers.catch_result = Some(catch_result);

    if let Some(finally_body) = finally_body {
        let mut finally_indices = local_indices_for_existing_spec(&spec);
        let (finally_expr, finally_error, finally_local_init_start, finally_local_init_count) =
            terminal_body_result(
                finally_body,
                &mut finally_indices,
                spec.frame.locals.len(),
                &mut spec.frame.post_wait_locals,
                None,
                error_message,
            )?;
        if finally_error.is_some() {
            return None;
        }
        spec.handlers.finally_expr = Some(finally_expr);
        spec.handlers.finally_local_init_start = finally_local_init_start;
        spec.handlers.finally_local_init_count = finally_local_init_count;
    }
    spec.sync_frame_handler_state();
    Some(spec)
}

fn emit_minimal_wait_module(spec: AsyncEmitterSpec, error_messages: Vec<String>) -> Vec<u8> {
    HEAP_GLOBAL.with(|g| *g.borrow_mut() = spec.heap_global());
    // Build the data section first so the per-string addresses are
    // available to the emit helpers. Populates the thread-local
    // `STRING_ADDR_MAP` for `string_literal_addr` to read and
    // `DICT_ADDR_MAP` for `dict_addr_for_message` to read.
    let data_bytes = build_string_data_section(&spec, &error_messages);

    let mut module = EncModule::new();

    let mut types = TypeSection::new();
    types.ty().function(vec![], vec![ValType::F64]);
    types.ty().function(vec![], vec![ValType::I32]);
    types.ty().function(vec![ValType::I32], vec![ValType::I32]);
    types.ty().function(vec![ValType::I32], vec![ValType::I64]);
    types.ty().function(vec![], vec![ValType::I64]);
    types
        .ty()
        .function(vec![ValType::I32, ValType::I32], vec![]);
    // Type 6: `__fai_str_eq(a: i64, b: i64) -> i32`
    types
        .ty()
        .function(vec![ValType::I64, ValType::I64], vec![ValType::I32]);
    module.section(&types);

    let mut imports = ImportSection::new();
    imports.import("env", "now_ms", EntityType::Function(0));
    imports.import("env", "host_set_timer", EntityType::Function(5));
    module.section(&imports);

    let mut funcs = FunctionSection::new();
    funcs.function(1);
    funcs.function(1);
    funcs.function(2);
    funcs.function(3);
    funcs.function(4);
    // FUNC_STR_EQ uses type 6.
    funcs.function(6);
    module.section(&funcs);

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memories);

    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    globals.global(
        GlobalType {
            val_type: ValType::F64,
            mutable: true,
            shared: false,
        },
        &ConstExpr::f64_const(0.0),
    );
    globals.global(
        GlobalType {
            val_type: ValType::I64,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i64_const(crate::runtime::VAL_VOID),
    );
    for _ in spec
        .frame
        .locals
        .iter()
        .chain(spec.frame.post_wait_locals.iter())
    {
        globals.global(
            GlobalType {
                val_type: ValType::I64,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i64_const(crate::runtime::VAL_VOID),
        );
    }
    for _ in &spec.children {
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
    }
    for _ in &spec.children {
        globals.global(
            GlobalType {
                val_type: ValType::F64,
                mutable: true,
                shared: false,
            },
            &ConstExpr::f64_const(0.0),
        );
    }
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(8),
    );
    module.section(&globals);

    let mut exports = ExportSection::new();
    exports.export("_start_async", ExportKind::Func, FUNC_START_ASYNC);
    exports.export("__fai_poll", ExportKind::Func, FUNC_POLL);
    exports.export("__fai_resume_task", ExportKind::Func, FUNC_RESUME_TASK);
    exports.export("__fai_task_result", ExportKind::Func, FUNC_TASK_RESULT);
    exports.export("_start", ExportKind::Func, FUNC_START_SYNC);
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("__heap_ptr", ExportKind::Global, spec.heap_global());
    module.section(&exports);

    let mut code = CodeSection::new();
    code.function(&emit_start_async(&spec));
    code.function(&emit_poll(&spec));
    code.function(&emit_resume_task());
    code.function(&emit_task_result(&spec));
    code.function(&emit_start_sync());
    code.function(&emit_str_eq());
    module.section(&code);

    // Data section: baked string constants and the Error dict.
    // The data is loaded into memory at offset `STRING_DATA_BASE`
    // (= 1024) at instantiation time. The whole module sees the
    // bytes immediately.
    if !data_bytes.is_empty() {
        let mut data = DataSection::new();
        data.active(
            0,
            &ConstExpr::i32_const(STRING_DATA_BASE as i32),
            data_bytes.iter().copied(),
        );
        module.section(&data);
    }

    module.finish()
}

fn emit_start_async(spec: &AsyncEmitterSpec) -> Function {
    let mut f = Function::new(Vec::new());
    for (idx, local) in spec.frame.locals.iter().enumerate() {
        emit_boxed_result_expr(&mut f, &local.value, None);
        f.instruction(&Instruction::GlobalSet(spec.local_global(idx)));
    }
    if !spec.children.is_empty() {
        for task in &spec.task_records {
            f.instruction(&Instruction::I32Const(STATUS_PENDING));
            f.instruction(&Instruction::GlobalSet(task.slot.state_global));
            f.instruction(&Instruction::Call(IMPORT_NOW_MS));
            f.instruction(&Instruction::F64Const(task.child.delay_ms));
            f.instruction(&Instruction::F64Add);
            f.instruction(&Instruction::GlobalSet(task.slot.wake_global));
            emit_host_set_timer(&mut f, task.slot.task_id, task.child.delay_ms);
        }
        if spec.root.complete_immediately {
            emit_boxed_result_expr(&mut f, &spec.root.result, None);
            f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
            f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
            f.instruction(&Instruction::GlobalSet(spec.frame.resume_state.global));
            f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
        } else {
            f.instruction(&Instruction::I32Const(STATUS_PENDING));
            f.instruction(&Instruction::GlobalSet(spec.frame.resume_state.global));
            f.instruction(&Instruction::I32Const(STATUS_PENDING));
        }
        f.instruction(&Instruction::End);
        return f;
    }
    f.instruction(&Instruction::Call(IMPORT_NOW_MS));
    f.instruction(&Instruction::F64Const(spec.root.delay_ms));
    f.instruction(&Instruction::F64Add);
    f.instruction(&Instruction::GlobalSet(spec.frame.root_wake.global));
    emit_host_set_timer(&mut f, 1, spec.root.delay_ms);
    f.instruction(&Instruction::I32Const(STATUS_PENDING));
    f.instruction(&Instruction::GlobalSet(spec.frame.resume_state.global));
    f.instruction(&Instruction::I32Const(STATUS_PENDING));
    f.instruction(&Instruction::End);
    f
}

fn emit_poll(spec: &AsyncEmitterSpec) -> Function {
    let mut f = Function::new(Vec::new());
    if !spec.children.is_empty() {
        for idx in 0..spec.children.len() {
            emit_child_poll(&mut f, spec, idx);
        }
        if !spec.root.complete_immediately {
            for idx in spec.child_indices_by_completion() {
                emit_root_fail_if_child_failed(&mut f, spec, idx);
            }
        }
        if !spec.root.complete_immediately {
            f.instruction(&Instruction::GlobalGet(spec.frame.resume_state.global));
            f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
            f.instruction(&Instruction::I32Ne);
            f.instruction(&Instruction::GlobalGet(spec.frame.resume_state.global));
            f.instruction(&Instruction::I32Const(STATUS_FAILED));
            f.instruction(&Instruction::I32Ne);
            f.instruction(&Instruction::I32And);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            emit_all_children_complete_condition(&mut f, spec);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            emit_local_initializers(
                &mut f,
                spec,
                spec.root.local_init_start,
                spec.root.local_init_count,
                None,
            );
            emit_local_initializers(
                &mut f,
                spec,
                spec.frame.handler_state.finally_local_init_start,
                spec.frame.handler_state.finally_local_init_count,
                None,
            );
            emit_boxed_result_with_finally(
                &mut f,
                &spec.root.result,
                spec.handlers.finally_expr.as_ref(),
                None,
            );
            f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
            f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
            f.instruction(&Instruction::GlobalSet(spec.frame.resume_state.global));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::GlobalGet(spec.frame.resume_state.global));
        f.instruction(&Instruction::End);
        return f;
    }
    f.instruction(&Instruction::GlobalGet(spec.frame.resume_state.global));
    f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I32,
    )));
    f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::Call(IMPORT_NOW_MS));
    f.instruction(&Instruction::GlobalGet(spec.frame.root_wake.global));
    f.instruction(&Instruction::F64Ge);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I32,
    )));
    let first_deferred = spec.first_deferred_local_idx();
    for (idx, local) in spec.frame.post_wait_locals.iter().enumerate() {
        let absolute_idx = spec.frame.locals.len() + idx;
        if absolute_idx >= first_deferred {
            continue;
        }
        emit_boxed_result_expr(&mut f, &local.value, None);
        f.instruction(&Instruction::GlobalSet(spec.local_global(absolute_idx)));
    }
    if let Some(error) = &spec.root.error {
        emit_root_error_or_catch(&mut f, spec, error);
    } else {
        emit_local_initializers(
            &mut f,
            spec,
            spec.frame.handler_state.finally_local_init_start,
            spec.frame.handler_state.finally_local_init_count,
            None,
        );
        emit_boxed_result_with_finally(
            &mut f,
            &spec.root.result,
            spec.handlers.finally_expr.as_ref(),
            None,
        );
        f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
        f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
    }
    f.instruction(&Instruction::GlobalSet(spec.frame.resume_state.global));
    f.instruction(&Instruction::GlobalGet(spec.frame.resume_state.global));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::I32Const(STATUS_PENDING));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

fn shift_result_expr_post_indices(
    expr: &mut ResultExpr,
    creation_local_len: usize,
    final_local_len: usize,
) {
    match expr {
        ResultExpr::Int(expr) => {
            shift_int_expr_post_indices(expr, creation_local_len, final_local_len)
        }
        ResultExpr::IfElse {
            condition,
            then_branch,
            else_branch,
        } => {
            shift_int_expr_post_indices(condition, creation_local_len, final_local_len);
            shift_result_expr_post_indices(then_branch, creation_local_len, final_local_len);
            shift_result_expr_post_indices(else_branch, creation_local_len, final_local_len);
        }
        ResultExpr::Tuple(items) => {
            for item in items {
                shift_result_expr_post_indices(item, creation_local_len, final_local_len);
            }
        }
        ResultExpr::String(_) => {}
    }
}

fn shift_int_expr_post_indices(
    expr: &mut IntExpr,
    creation_local_len: usize,
    final_local_len: usize,
) {
    match expr {
        IntExpr::Local(idx) if *idx >= creation_local_len => {
            *idx += final_local_len - creation_local_len;
        }
        IntExpr::Binary { left, right, .. } => {
            shift_int_expr_post_indices(left, creation_local_len, final_local_len);
            shift_int_expr_post_indices(right, creation_local_len, final_local_len);
        }
        IntExpr::TupleIndex {
            tuple_local_idx, ..
        } if *tuple_local_idx >= creation_local_len => {
            *tuple_local_idx += final_local_len - creation_local_len;
        }
        IntExpr::Literal(_) | IntExpr::Local(_) | IntExpr::StringEq { .. } => {}
        IntExpr::TupleIndex { .. } => {}
    }
}

fn emit_local_initializers(
    f: &mut Function,
    spec: &AsyncEmitterSpec,
    start: usize,
    count: usize,
    catch_local: Option<usize>,
) {
    for idx in start..start.saturating_add(count) {
        if idx < spec.frame.locals.len() {
            emit_boxed_result_expr(f, &spec.frame.locals[idx].value, catch_local);
        } else {
            let post_idx = idx - spec.frame.locals.len();
            let Some(local) = spec.frame.post_wait_locals.get(post_idx) else {
                continue;
            };
            emit_boxed_result_expr(f, &local.value, catch_local);
        }
        f.instruction(&Instruction::GlobalSet(spec.local_global(idx)));
    }
}

fn emit_child_poll(f: &mut Function, spec: &AsyncEmitterSpec, child_idx: usize) {
    let task = &spec.task_records[child_idx];
    f.instruction(&Instruction::GlobalGet(task.slot.state_global));
    f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Call(IMPORT_NOW_MS));
    f.instruction(&Instruction::GlobalGet(task.slot.wake_global));
    f.instruction(&Instruction::F64Ge);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    if let Some(error) = task.child.error.as_ref() {
        emit_boxed_throw_value(f, error);
        f.instruction(&Instruction::GlobalSet(
            spec.local_global(task.slot.result_local_idx),
        ));
        f.instruction(&Instruction::I32Const(STATUS_FAILED));
        f.instruction(&Instruction::GlobalSet(task.slot.state_global));
    } else {
        emit_local_initializers(
            f,
            spec,
            task.slot.local_init_start,
            task.slot.local_init_count,
            None,
        );
        f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
        f.instruction(&Instruction::GlobalSet(task.slot.state_global));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
}

fn emit_root_fail_if_child_failed(f: &mut Function, spec: &AsyncEmitterSpec, child_idx: usize) {
    let task = &spec.task_records[child_idx];
    f.instruction(&Instruction::GlobalGet(spec.frame.resume_state.global));
    f.instruction(&Instruction::I32Const(STATUS_FAILED));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::GlobalGet(task.slot.state_global));
    f.instruction(&Instruction::I32Const(STATUS_FAILED));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::GlobalGet(
        spec.local_global(task.slot.result_local_idx),
    ));
    if let (Some(catch_idx), Some(catch_result)) = (
        spec.frame.handler_state.catch_error_local_idx,
        spec.handlers.catch_result.as_ref(),
    ) {
        f.instruction(&Instruction::GlobalSet(spec.local_global(catch_idx)));
        emit_local_initializers(
            f,
            spec,
            spec.frame.handler_state.catch_local_init_start,
            spec.frame.handler_state.catch_local_init_count,
            spec.frame.handler_state.catch_error_local_idx,
        );
        emit_local_initializers(
            f,
            spec,
            spec.frame.handler_state.finally_local_init_start,
            spec.frame.handler_state.finally_local_init_count,
            spec.frame.handler_state.catch_error_local_idx,
        );
        emit_boxed_result_with_finally(
            f,
            catch_result,
            spec.handlers.finally_expr.as_ref(),
            spec.frame.handler_state.catch_error_local_idx,
        );
        f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
        f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
    } else {
        f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
        f.instruction(&Instruction::I32Const(STATUS_FAILED));
    }
    f.instruction(&Instruction::GlobalSet(spec.frame.resume_state.global));
    f.instruction(&Instruction::End);
}

fn emit_root_error_or_catch(f: &mut Function, spec: &AsyncEmitterSpec, error: &ThrowValue) {
    if let (Some(catch_idx), Some(catch_result)) = (
        spec.frame.handler_state.catch_error_local_idx,
        spec.handlers.catch_result.as_ref(),
    ) {
        emit_boxed_throw_value(f, error);
        f.instruction(&Instruction::GlobalSet(spec.local_global(catch_idx)));
        emit_local_initializers(
            f,
            spec,
            spec.frame.handler_state.catch_local_init_start,
            spec.frame.handler_state.catch_local_init_count,
            spec.frame.handler_state.catch_error_local_idx,
        );
        emit_local_initializers(
            f,
            spec,
            spec.frame.handler_state.finally_local_init_start,
            spec.frame.handler_state.finally_local_init_count,
            spec.frame.handler_state.catch_error_local_idx,
        );
        emit_boxed_result_with_finally(
            f,
            catch_result,
            spec.handlers.finally_expr.as_ref(),
            spec.frame.handler_state.catch_error_local_idx,
        );
        f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
        f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
    } else {
        emit_boxed_throw_value(f, error);
        f.instruction(&Instruction::GlobalSet(GLOBAL_RESULT));
        f.instruction(&Instruction::I32Const(STATUS_FAILED));
    }
}

fn emit_all_children_complete_condition(f: &mut Function, spec: &AsyncEmitterSpec) {
    for (idx, task) in spec.task_records.iter().enumerate() {
        f.instruction(&Instruction::GlobalGet(task.slot.state_global));
        f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
        f.instruction(&Instruction::I32Eq);
        if idx > 0 {
            f.instruction(&Instruction::I32And);
        }
    }
}

fn emit_host_set_timer(f: &mut Function, task_id: i32, delay_ms: f64) {
    f.instruction(&Instruction::I32Const(task_id));
    f.instruction(&Instruction::I32Const(delay_ms.max(0.0) as i32));
    f.instruction(&Instruction::Call(IMPORT_HOST_SET_TIMER));
}

fn emit_boxed_int_expr(f: &mut Function, expr: &IntExpr, catch_local: Option<usize>) {
    emit_raw_int_expr(f, expr, catch_local);
    f.instruction(&Instruction::I64ExtendI32U);
    f.instruction(&Instruction::I64Const(
        crate::runtime::QNAN | crate::runtime::TAG_INT,
    ));
    f.instruction(&Instruction::I64Or);
}

/// Emit a boxed NaN-boxed value for any `ResultExpr` — int or
/// string. Strings push a constant boxed pointer that points into
/// the data section; the bytes are baked at module-build time.
/// `catch_local` is the index of the catch binding in the
/// post-wait locals when emitting inside a catch body (needed
/// for `e.message` field reads).
fn emit_boxed_result_expr(f: &mut Function, expr: &ResultExpr, catch_local: Option<usize>) {
    match expr {
        ResultExpr::Int(i) => emit_boxed_int_expr(f, i, catch_local),
        ResultExpr::String(s) => emit_boxed_string_expr(f, s, catch_local),
        ResultExpr::IfElse {
            condition,
            then_branch,
            else_branch,
        } => {
            // Evaluate the condition as a boxed int. The branch
            // selector is the low 32 bits of the boxed i64 (the
            // int value). For a value of 0, take the else
            // branch; otherwise take the then branch. The
            // result block is `Result(i64)` because both
            // branches leave a boxed i64 on the stack.
            emit_boxed_int_expr(f, condition, catch_local);
            f.instruction(&Instruction::I64Const(0x0000_0000_FFFFFFFF_u64 as i64));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                ValType::I64,
            )));
            emit_boxed_result_expr(f, then_branch, catch_local);
            f.instruction(&Instruction::Else);
            emit_boxed_result_expr(f, else_branch, catch_local);
            f.instruction(&Instruction::End);
        }
        ResultExpr::Tuple(items) => emit_boxed_tuple_expr(f, items, catch_local),
    }
}

fn emit_boxed_tuple_expr(f: &mut Function, items: &[ResultExpr], catch_local: Option<usize>) {
    let count = items.len() as i32;
    let size = 8 + count * 8;
    let heap_global = current_heap_global();
    for (idx, item) in items.iter().enumerate() {
        f.instruction(&Instruction::GlobalGet(heap_global));
        emit_boxed_result_expr(f, item, catch_local);
        f.instruction(&Instruction::I64Store(wasm_encoder::MemArg {
            offset: 8 + (idx as u64) * 8,
            align: 3,
            memory_index: 0,
        }));
    }
    f.instruction(&Instruction::GlobalGet(heap_global));
    f.instruction(&Instruction::I32Const(crate::runtime::OBJ_TAG_TUPLE));
    f.instruction(&Instruction::I32Store(mem0_i32()));
    f.instruction(&Instruction::GlobalGet(heap_global));
    f.instruction(&Instruction::I32Const(count));
    f.instruction(&Instruction::I32Store(mem4_i32()));
    f.instruction(&Instruction::GlobalGet(heap_global));
    f.instruction(&Instruction::I64ExtendI32U);
    f.instruction(&Instruction::I64Const(
        crate::runtime::QNAN | crate::runtime::SIGN_BIT,
    ));
    f.instruction(&Instruction::I64Or);
    f.instruction(&Instruction::GlobalGet(heap_global));
    f.instruction(&Instruction::I32Const(size));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalSet(heap_global));
}

/// Emit a boxed i64 for a `ThrowValue`. `IntLiteral` boxes the int
/// via the normal int tag; `ErrorDict` pushes the constant boxed
/// Error dict pointer (which points into the data section).
fn emit_boxed_throw_value(f: &mut Function, expr: &ThrowValue) {
    match expr {
        ThrowValue::IntLiteral(value) => {
            f.instruction(&Instruction::I32Const(*value));
            f.instruction(&Instruction::I64ExtendI32U);
            f.instruction(&Instruction::I64Const(
                crate::runtime::QNAN | crate::runtime::TAG_INT,
            ));
            f.instruction(&Instruction::I64Or);
        }
        ThrowValue::ErrorDict(message) => {
            // Push the boxed pointer of the dict for THIS message.
            // The dict address was resolved at module build time by
            // `build_string_data_section` and stashed in
            // `DICT_ADDR_MAP`.
            let dict_addr = dict_addr_for_message(message);
            f.instruction(&Instruction::I64Const(boxed_obj_ptr(dict_addr) as i64));
        }
    }
}

/// Emit a boxed String. `Literal` pushes a constant boxed pointer
/// into the data section. `ErrorMessage` is the catch body's
/// `e.message` form — it reads the dict's value field at runtime
/// (so it works regardless of which `Error('msg')` was thrown).
/// The runtime field read is: push the catch binding (a boxed
/// dict pointer), mask off the object tag to get the dict's
/// heap address, add 16, and load the 8-byte boxed value pointer.
fn emit_boxed_string_expr(f: &mut Function, expr: &StringExpr, catch_local: Option<usize>) {
    match expr {
        StringExpr::Literal(s) => {
            let addr = string_literal_addr(s);
            f.instruction(&Instruction::I64Const(boxed_obj_ptr(addr) as i64));
        }
        StringExpr::ErrorMessage => {
            let local_idx = catch_local.expect(
                "e.message emission requires a catch binding local; \
                 check that the result slot is inside a catch body",
            );
            // Push the catch binding (a boxed i64 dict pointer).
            f.instruction(&Instruction::GlobalGet(frame_local_global(local_idx)));
            // Mask off the object tag → dict_addr.
            f.instruction(&Instruction::I64Const(0x0000_FFFF_FFFF_FFFF_u64 as i64));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I32WrapI64);
            // Load 8 bytes from `dict_addr + 16` (the boxed value
            // field).
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I64Load(mem0_i64()));
        }
    }
}

/// Emit the boxed integer `base + finally` when `finally` is `Some`,
/// otherwise just `base`. Leaves the boxed i64 on the stack. Used to
/// apply the narrow-path `finally` additively to whatever result the
/// try/catch path produced. If `base` is a string, the finally
/// contribution is dropped (the additive semantic only makes sense
/// for ints). If `finally` is `Some` but holds a String rather than
/// an Int, it's also dropped — the narrow path only allows int
/// finally bodies in this slice.
fn emit_boxed_result_with_finally(
    f: &mut Function,
    base: &ResultExpr,
    finally: Option<&ResultExpr>,
    catch_local: Option<usize>,
) {
    let finally_int = finally.and_then(|r| match r {
        ResultExpr::Int(i) => Some(i),
        ResultExpr::String(_) | ResultExpr::Tuple(_) | ResultExpr::IfElse { .. } => None,
    });
    match (base, finally_int) {
        (ResultExpr::Int(i), Some(fin)) => {
            let combined = IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(i.clone()),
                right: Box::new(fin.clone()),
            };
            emit_boxed_int_expr(f, &combined, catch_local);
        }
        (ResultExpr::Int(i), None) => emit_boxed_int_expr(f, i, catch_local),
        (ResultExpr::String(s), _) => emit_boxed_string_expr(f, s, catch_local),
        (ResultExpr::Tuple(items), _) => emit_boxed_tuple_expr(f, items, catch_local),
        (ResultExpr::IfElse { .. }, _) => {
            // Finally is incompatible with an if/else base
            // (would require evaluating the finally around a
            // branched result). Fall through to the base
            // emission without the finally add-on.
            emit_boxed_result_expr(f, base, catch_local);
        }
    }
}

fn emit_raw_int_expr(f: &mut Function, expr: &IntExpr, catch_local: Option<usize>) {
    match expr {
        IntExpr::Literal(value) => {
            f.instruction(&Instruction::I32Const(*value));
        }
        IntExpr::Local(idx) => {
            f.instruction(&Instruction::GlobalGet(frame_local_global(*idx)));
            f.instruction(&Instruction::I32WrapI64);
        }
        IntExpr::Binary { op, left, right } => {
            emit_raw_int_expr(f, left, catch_local);
            emit_raw_int_expr(f, right, catch_local);
            match op {
                IntBinaryOp::Add => f.instruction(&Instruction::I32Add),
                IntBinaryOp::Sub => f.instruction(&Instruction::I32Sub),
                IntBinaryOp::Mul => f.instruction(&Instruction::I32Mul),
            };
        }
        IntExpr::TupleIndex {
            tuple_local_idx,
            index,
        } => {
            f.instruction(&Instruction::GlobalGet(frame_local_global(
                *tuple_local_idx,
            )));
            f.instruction(&Instruction::I64Const(0x0000_FFFF_FFFF_FFFF_u64 as i64));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I64Load(wasm_encoder::MemArg {
                offset: 8 + (*index as u64) * 8,
                align: 3,
                memory_index: 0,
            }));
            f.instruction(&Instruction::I32WrapI64);
        }
        IntExpr::StringEq { left, right } => {
            // Push the two boxed strings, then call the
            // str_eq helper which leaves 0 or 1 on the stack.
            // The catch_local is threaded through so the
            // operands can reference `e.message` if needed.
            emit_boxed_string_expr(f, left, catch_local);
            emit_boxed_string_expr(f, right, catch_local);
            f.instruction(&Instruction::Call(FUNC_STR_EQ));
        }
    }
}

fn emit_resume_task() -> Function {
    let mut f = Function::new(vec![(1, ValType::I32)]);
    f.instruction(&Instruction::Call(FUNC_POLL));
    f.instruction(&Instruction::End);
    f
}

/// `__fai_str_eq(a: i64, b: i64) -> i32` — boxed-string equality
/// helper. Returns 1 when both strings are equal, 0 otherwise.
/// The narrow path uses this for the catch-body `e.message ==
/// 'foo'` form without pulling in the full runtime. Locals
/// (after the 2 i64 parameters):
///   2: a_addr (i32)
///   3: a_len (i32)
///   4: b_addr (i32)
///   5: b_len (i32)
///   6: i (loop counter, i32)
fn emit_str_eq() -> Function {
    // 5 i32 scratch locals (i32 locals 0..4 follow the 2 i64 params).
    let mut f = Function::new(vec![(5, ValType::I32)]);
    // a_addr = a & MASK; a_len = mem[a_addr + 4]
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(0x0000_FFFF_FFFF_FFFF_u64 as i64));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3));
    // b_addr = b & MASK; b_len = mem[b_addr + 4]
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Const(0x0000_FFFF_FFFF_FFFF_u64 as i64));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(5));
    // if a_len != b_len: return 0
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // i = 0
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(6));
    // while i < a_len:
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // if mem[a_addr+i] != mem[b_addr+i]: return 0
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(wasm_encoder::MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(wasm_encoder::MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(6));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    // Fall through: return 1 (all bytes matched)
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::End);
    f
}

fn emit_task_result(spec: &AsyncEmitterSpec) -> Function {
    let mut f = Function::new(Vec::new());
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::GlobalGet(GLOBAL_RESULT));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    for task in &spec.task_records {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(task.slot.task_id));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::GlobalGet(
            spec.local_global(task.slot.result_local_idx),
        ));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::I64Const(crate::runtime::VAL_VOID));
    f.instruction(&Instruction::End);
    f
}

fn emit_start_sync() -> Function {
    let mut f = Function::new(vec![(1, ValType::I32)]);
    f.instruction(&Instruction::Call(FUNC_START_ASYNC));
    f.instruction(&Instruction::Drop);
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Call(FUNC_POLL));
    f.instruction(&Instruction::LocalSet(0));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(STATUS_FAILED));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(STATUS_COMPLETE));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::BrIf(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::Call(FUNC_TASK_RESULT));
    f.instruction(&Instruction::End);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Program {
        fai_compiler::prepare_source(source, None)
            .expect("source should parse")
            .serde_ast
    }

    fn import_names(wasm: &[u8]) -> Vec<String> {
        let parser = wasmparser::Parser::new(0);
        let mut names = Vec::new();
        for payload in parser.parse_all(wasm) {
            if let wasmparser::Payload::ImportSection(section) = payload.expect("payload") {
                for import in section {
                    let import = import.expect("import");
                    names.push(import.name.to_string());
                }
                break;
            }
        }
        names
    }

    #[test]
    fn extracts_simple_wait_then_int_main() {
        let ast = parse("def main\n    @return Int\ndo\n  sleep(1)\n  42\nend\n");
        let spec = extract_minimal_wait_main(&ast, None).expect("minimal wait main");
        assert_eq!(spec.root.delay_ms, 1.0);
        assert!(spec.frame.locals.is_empty());
        assert!(spec.frame.post_wait_locals.is_empty());
        assert_eq!(spec.root.result, ResultExpr::Int(IntExpr::Literal(42)));
    }

    #[test]
    fn async_wait_imports_scheduler_timer_not_legacy_sleep() {
        let ast = parse("def main\n    @return Int\ndo\n  sleep(1)\n  42\nend\n");
        let wasm = try_codegen_minimal_wait_main(&ast).expect("async wait wasm");
        let imports = import_names(&wasm);
        assert!(imports.iter().any(|name| name == "host_set_timer"));
        assert!(!imports.iter().any(|name| name == "sleep_ms"));
    }

    #[test]
    fn extracts_locals_live_across_wait() {
        let ast = parse("def main\n    @return Int\ndo\n  sleep(1)\n  let x = 42\n  x\nend\n");
        assert!(extract_minimal_wait_main(&ast, None).is_none());
        let ast = parse("def main\n    @return Int\ndo\n  let x = 41\n  sleep(1)\n  x + 1\nend\n");
        let spec = extract_minimal_wait_main(&ast, None).expect("minimal wait main with local");
        assert_eq!(spec.frame.locals.len(), 1);
        assert_eq!(spec.frame.locals[0].name, "x");
        assert_eq!(
            spec.frame.locals[0].value,
            ResultExpr::Int(IntExpr::Literal(41))
        );
        assert!(spec.frame.post_wait_locals.is_empty());
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(0)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
    }

    #[test]
    fn extracts_throw_after_wait_as_root_error() {
        let ast = parse("def main\n    @return Int\ndo\n  sleep(1)\n  throw 7\nend\n");
        let spec = extract_minimal_wait_main(&ast, None).expect("minimal wait throw");
        assert_eq!(spec.root.result, ResultExpr::Int(IntExpr::Literal(0)));
        assert_eq!(spec.root.error, Some(ThrowValue::IntLiteral(7)));
    }

    #[test]
    fn extracts_try_catch_after_wait_throw() {
        let ast = parse(
            "def main\n    @return Int\ndo\n  try\n    sleep(1)\n    throw 7\n  catch e\n    e + 1\n  end\nend\n",
        );
        let spec = extract_minimal_try_main(&ast, None).expect("minimal try catch wait");
        assert_eq!(spec.root.error, Some(ThrowValue::IntLiteral(7)));
        assert_eq!(spec.handlers.catch_error_local_idx, Some(0));
        assert_eq!(
            spec.handlers.catch_result,
            Some(ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(0)),
                right: Box::new(IntExpr::Literal(1)),
            }))
        );
    }

    #[test]
    fn rejects_statements_after_wait_before_result() {
        let ast = parse("def main\n    @return Int\ndo\n  sleep(1)\n  let x = 42\n  x\nend\n");
        assert!(extract_minimal_wait_main(&ast, None).is_none());
    }

    #[test]
    fn lowerer_accepts_post_wait_lets_before_result() {
        let ast = parse(
            "def main\n    @return Int\ndo\n  let x = 40\n  sleep(1)\n  let y = x + 1\n  y + 1\nend\n",
        );
        let spec = lower_async_main(&ast, None).expect("lowered wait body");
        assert_eq!(spec.root.delay_ms, 1.0);
        assert_eq!(spec.frame.locals.len(), 1);
        assert_eq!(spec.frame.post_wait_locals.len(), 1);
        assert_eq!(spec.frame.post_wait_locals[0].name, "y");
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(1)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
    }

    #[test]
    fn lowerer_accepts_post_auto_await_lets_before_result() {
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  7\nend\n\n\
             def main\n    @return Int\ndo\n  let x = child()\n  let y = x + 1\n  y + 1\nend\n",
        );
        let spec = lower_async_main(&ast, None).expect("lowered auto-await body");
        assert_eq!(spec.root.delay_ms, 1.0);
        assert_eq!(spec.frame.post_wait_locals.len(), 2);
        assert_eq!(spec.frame.post_wait_locals[0].name, "x");
        assert_eq!(spec.frame.post_wait_locals[1].name, "y");
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(1)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
    }

    #[test]
    fn lowerer_accepts_multiple_sequential_auto_awaits() {
        let ast = parse(
            "def first\n    @return Int\ndo\n  sleep(1)\n  2\nend\n\n\
             def second\n    @param x Int\n    @return Int\ndo\n  sleep(2)\n  x + 3\nend\n\n\
             def main\n    @return Int\ndo\n  let a = first()\n  let b = second(a)\n  let c = b + 4\n  c\nend\n",
        );
        let spec = lower_async_main(&ast, None).expect("lowered sequential auto-awaits");
        assert_eq!(spec.root.delay_ms, 3.0);
        assert!(spec.frame.locals.is_empty());
        assert_eq!(spec.frame.post_wait_locals.len(), 4);
        assert_eq!(spec.frame.post_wait_locals[0].name, "a");
        assert_eq!(spec.frame.post_wait_locals[1].name, "second.x");
        assert_eq!(spec.frame.post_wait_locals[2].name, "b");
        assert_eq!(spec.frame.post_wait_locals[3].name, "c");
        assert_eq!(
            spec.frame.post_wait_locals[1].value,
            ResultExpr::Int(IntExpr::Local(0))
        );
        assert_eq!(spec.root.result, ResultExpr::Int(IntExpr::Local(3)));
    }

    #[test]
    fn extracts_simple_auto_await_call() {
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  7\nend\n\n\
             def main\n    @return Int\ndo\n  let x = child()\n  x + 1\nend\n",
        );
        let spec = extract_minimal_auto_await_main(&ast, None).expect("minimal auto-await main");
        assert!(spec.frame.locals.is_empty());
        assert_eq!(spec.frame.post_wait_locals.len(), 1);
        assert_eq!(spec.frame.post_wait_locals[0].name, "x");
        assert_eq!(
            spec.frame.post_wait_locals[0].value,
            ResultExpr::Int(IntExpr::Literal(7))
        );
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(0)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
    }

    #[test]
    fn extracts_auto_await_child_throw_as_root_error() {
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  throw 7\nend\n\n\
             def main\n    @return Int\ndo\n  let x = child()\n  x + 1\nend\n",
        );
        let spec = extract_minimal_auto_await_main(&ast, None).expect("minimal auto-await throw");
        assert_eq!(spec.root.error, Some(ThrowValue::IntLiteral(7)));
    }

    #[test]
    fn extracts_try_catch_auto_await_child_throw() {
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  throw 7\nend\n\n\
             def main\n    @return Int\ndo\n  try\n    let x = child()\n    x + 1\n  catch e\n    e + 1\n  end\nend\n",
        );
        let spec = extract_minimal_try_main(&ast, None).expect("minimal try catch auto-await");
        assert_eq!(spec.root.error, Some(ThrowValue::IntLiteral(7)));
        assert_eq!(spec.handlers.catch_error_local_idx, Some(1));
    }

    #[test]
    fn extracts_auto_await_with_int_argument() {
        let ast = parse(
            "def child\n    @param x Int\n    @return Int\ndo\n  sleep(1)\n  x + 1\nend\n\n\
             def main\n    @return Int\ndo\n  let y = child(7)\n  y + 1\nend\n",
        );
        let spec = extract_minimal_auto_await_main(&ast, None).expect("minimal auto-await arg");
        assert_eq!(spec.frame.locals.len(), 1);
        assert_eq!(spec.frame.locals[0].name, "child.x");
        assert_eq!(
            spec.frame.locals[0].value,
            ResultExpr::Int(IntExpr::Literal(7))
        );
        assert_eq!(
            spec.frame.post_wait_locals[0].value,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(0)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(1)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
    }

    #[test]
    fn extracts_auto_await_argument_from_caller_local() {
        let ast = parse(
            "def child\n    @param x Int\n    @return Int\ndo\n  sleep(1)\n  x + 1\nend\n\n\
             def main\n    @return Int\ndo\n  let base = 7\n  let y = child(base)\n  y + 1\nend\n",
        );
        let spec = extract_minimal_auto_await_main(&ast, None).expect("caller local arg");
        assert_eq!(spec.frame.locals.len(), 2);
        assert_eq!(spec.frame.locals[0].name, "base");
        assert_eq!(
            spec.frame.locals[0].value,
            ResultExpr::Int(IntExpr::Literal(7))
        );
        assert_eq!(spec.frame.locals[1].name, "child.x");
        assert_eq!(
            spec.frame.locals[1].value,
            ResultExpr::Int(IntExpr::Local(0))
        );
    }

    #[test]
    fn extracts_nested_auto_await_chain() {
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  7\nend\n\n\
             def parent\n    @return Int\ndo\n  let x = child()\n  x + 1\nend\n\n\
             def main\n    @return Int\ndo\n  let y = parent()\n  y + 1\nend\n",
        );
        let spec = extract_minimal_auto_await_main(&ast, None).expect("nested auto-await");
        assert_eq!(spec.root.delay_ms, 1.0);
        assert_eq!(spec.frame.post_wait_locals.len(), 2);
        assert_eq!(spec.frame.post_wait_locals[0].name, "parent.x");
        assert_eq!(spec.frame.post_wait_locals[1].name, "y");
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(1)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
    }

    #[test]
    fn extracts_minimal_all_children() {
        let ast = parse(
            "def slow\n    @return Int\ndo\n  sleep(50)\n  1\nend\n\n\
             def fast\n    @return Int\ndo\n  sleep(10)\n  2\nend\n\n\
             def main\n    @return Int\ndo\n  let a, b = all(slow(), fast())\n  a + b\nend\n",
        );
        let spec = extract_minimal_all_main(&ast, None).expect("minimal all");
        assert!(spec.frame.locals.is_empty());
        assert_eq!(spec.frame.post_wait_locals.len(), 2);
        assert_eq!(spec.child_delays_ms(), vec![50.0, 10.0]);
        assert!(!spec.root.complete_immediately);
        assert_eq!(spec.frame.post_wait_locals[0].name, "a");
        assert_eq!(
            spec.frame.post_wait_locals[0].value,
            ResultExpr::Int(IntExpr::Literal(1))
        );
        assert_eq!(spec.frame.post_wait_locals[1].name, "b");
        assert_eq!(
            spec.frame.post_wait_locals[1].value,
            ResultExpr::Int(IntExpr::Literal(2))
        );
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(0)),
                right: Box::new(IntExpr::Local(1)),
            })
        );
    }

    #[test]
    fn async_all_imports_scheduler_timer_not_legacy_run_all() {
        let ast = parse(
            "def slow\n    @return Int\ndo\n  sleep(50)\n  1\nend\n\n\
             def fast\n    @return Int\ndo\n  sleep(10)\n  2\nend\n\n\
             def main\n    @return Int\ndo\n  let a, b = all(slow(), fast())\n  a + b\nend\n",
        );
        let wasm = try_codegen_minimal_wait_main(&ast).expect("async all wasm");
        let imports = import_names(&wasm);
        assert!(imports.iter().any(|name| name == "host_set_timer"));
        assert!(!imports.iter().any(|name| name == "run_all"));
        assert!(!imports.iter().any(|name| name == "sleep_ms"));
    }

    #[test]
    fn extracts_minimal_all_child_throw() {
        let ast = parse(
            "def slow\n    @return Int\ndo\n  sleep(50)\n  1\nend\n\n\
             def fast\n    @return Int\ndo\n  sleep(10)\n  throw 9\nend\n\n\
             def main\n    @return Int\ndo\n  let a, b = all(slow(), fast())\n  a + b\nend\n",
        );
        let spec = extract_minimal_all_main(&ast, None).expect("minimal all throw");
        assert_eq!(
            spec.child_errors(),
            vec![None, Some(ThrowValue::IntLiteral(9))]
        );
    }

    #[test]
    fn extracts_try_catch_all_child_throw() {
        let ast = parse(
            "def slow\n    @return Int\ndo\n  sleep(50)\n  1\nend\n\n\
             def fast\n    @return Int\ndo\n  sleep(10)\n  throw 9\nend\n\n\
             def main\n    @return Int\ndo\n  try\n    let a, b = all(slow(), fast())\n    a + b\n  catch e\n    e + 1\n  end\nend\n",
        );
        let spec = extract_minimal_try_main(&ast, None).expect("minimal try catch all");
        assert_eq!(
            spec.child_errors(),
            vec![None, Some(ThrowValue::IntLiteral(9))]
        );
        assert_eq!(spec.handlers.catch_error_local_idx, Some(2));
        assert_eq!(spec.frame.handler_state.catch_error_local_idx, Some(2));
        assert_eq!(spec.frame.pending_tasks[0].state_global, 6);
        assert_eq!(spec.frame.pending_tasks[0].wake_global, 8);
        assert_eq!(spec.frame.pending_tasks[1].state_global, 7);
        assert_eq!(spec.frame.pending_tasks[1].wake_global, 9);
        assert_eq!(spec.frame.handler_state.catch_local_init_start, 3);
        assert_eq!(spec.frame.handler_state.catch_local_init_count, 0);
        assert_eq!(spec.frame.handler_state.finally_local_init_start, 0);
        assert_eq!(spec.frame.handler_state.finally_local_init_count, 0);
    }

    #[test]
    fn extracts_try_catch_all_distinct_error_messages() {
        let ast = parse(
            "def slow\n    @return Int\ndo\n  sleep(20)\n  throw Error('slow')\nend\n\n\
             def fast\n    @return Int\ndo\n  sleep(1)\n  throw Error('fast')\nend\n\n\
             def main\n    @return String\ndo\n  try\n    let a, b = all(slow(), fast())\n    'bad'\n  catch e\n    e.message\n  end\nend\n",
        );
        let error_messages = collect_unique_error_messages(&ast);
        let spec = extract_minimal_try_main(&ast, error_messages.first().map(String::as_str))
            .expect("all with distinct Error messages");
        assert_eq!(
            spec.child_errors(),
            vec![
                Some(ThrowValue::ErrorDict("slow".to_string())),
                Some(ThrowValue::ErrorDict("fast".to_string()))
            ]
        );
    }

    #[test]
    fn extracts_try_catch_all_distinct_error_messages_with_private_tests() {
        let ast = parse(
            "def main\n    @return String\ndo\n  try\n    let a, b = all(slow(), fast())\n    'bad'\n  catch e\n    e.message\n  end\nend\n\n\
private:\n\n\
def slow\n    @return Int\ndo\n  sleep(20)\n  throw Error('slow')\nend\n\n\
test slow\n  it 'covered'\n    print('placeholder')\n  end\nend\n\n\
def fast\n    @return Int\ndo\n  sleep(1)\n  throw Error('fast')\nend\n\n\
test fast\n  it 'covered'\n    print('placeholder')\n  end\nend\n",
        );
        let error_messages = collect_unique_error_messages(&ast);
        extract_minimal_try_main(&ast, error_messages.first().map(String::as_str))
            .expect("private/test all with distinct Error messages");
    }

    #[test]
    fn extracts_minimal_all_children_with_arguments() {
        let ast = parse(
            "def slow\n    @param x Int\n    @return Int\ndo\n  sleep(50)\n  x + 1\nend\n\n\
             def fast\n    @param x Int\n    @return Int\ndo\n  sleep(10)\n  x + 2\nend\n\n\
             def main\n    @return Int\ndo\n  let base = 10\n  let a, b = all(slow(base), fast(base))\n  a + b\nend\n",
        );
        let spec = extract_minimal_all_main(&ast, None).expect("minimal all with args");
        assert_eq!(spec.frame.locals.len(), 3);
        assert_eq!(spec.frame.locals[0].name, "base");
        assert_eq!(spec.frame.locals[1].name, "slow.x");
        assert_eq!(
            spec.frame.locals[1].value,
            ResultExpr::Int(IntExpr::Local(0))
        );
        assert_eq!(spec.frame.locals[2].name, "fast.x");
        assert_eq!(
            spec.frame.locals[2].value,
            ResultExpr::Int(IntExpr::Local(0))
        );
        assert_eq!(spec.frame.post_wait_locals.len(), 2);
        assert_eq!(
            spec.frame.post_wait_locals[0].value,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(1)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
        assert_eq!(
            spec.frame.post_wait_locals[1].value,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(2)),
                right: Box::new(IntExpr::Literal(2)),
            })
        );
    }

    #[test]
    fn extracts_all_children_with_nested_auto_await() {
        let ast = parse(
            "def slow_leaf\n    @return Int\ndo\n  sleep(50)\n  1\nend\n\n\
             def slow\n    @return Int\ndo\n  let x = slow_leaf()\n  x + 10\nend\n\n\
             def fast_leaf\n    @return Int\ndo\n  sleep(10)\n  2\nend\n\n\
             def fast\n    @return Int\ndo\n  let x = fast_leaf()\n  x + 20\nend\n\n\
             def main\n    @return Int\ndo\n  let a, b = all(slow(), fast())\n  a + b\nend\n",
        );
        let spec = extract_minimal_all_main(&ast, None).expect("nested all auto-await");
        assert_eq!(spec.child_delays_ms(), vec![50.0, 10.0]);
        assert_eq!(spec.child_result_local_indices(), vec![1, 3]);
        assert_eq!(spec.child_local_init_ranges(), vec![(0, 2), (2, 2)]);
        assert_eq!(spec.frame.post_wait_locals[0].name, "slow.x");
        assert_eq!(spec.frame.post_wait_locals[1].name, "a");
        assert_eq!(spec.frame.post_wait_locals[2].name, "fast.x");
        assert_eq!(spec.frame.post_wait_locals[3].name, "b");
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(1)),
                right: Box::new(IntExpr::Local(3)),
            })
        );
    }

    #[test]
    fn lowerer_accepts_all_tuple_binding() {
        let ast = parse(
            "def slow\n    @return Int\ndo\n  sleep(50)\n  1\nend\n\n\
             def fast\n    @return Int\ndo\n  sleep(10)\n  2\nend\n\n\
             def main\n    @return Int\ndo\n  let results = all(slow(), fast())\n  results[0] + results[1]\nend\n",
        );
        let spec = lower_async_main(&ast, None).expect("all tuple binding");
        assert_eq!(spec.children.len(), 2);
        assert_eq!(spec.root.local_init_count, 1);
        assert_eq!(spec.frame.post_wait_locals[2].name, "results");
        assert_eq!(
            spec.frame.post_wait_locals[2].value,
            ResultExpr::Tuple(vec![
                ResultExpr::Int(IntExpr::Local(0)),
                ResultExpr::Int(IntExpr::Local(1)),
            ])
        );
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::TupleIndex {
                    tuple_local_idx: 2,
                    index: 0,
                }),
                right: Box::new(IntExpr::TupleIndex {
                    tuple_local_idx: 2,
                    index: 1,
                }),
            })
        );
    }

    #[test]
    fn groups_async_emitter_spec_into_frame_root_children_and_handlers() {
        let ast = parse(
            "def slow\n    @param x Int\n    @return Int\ndo\n  sleep(50)\n  x + 1\nend\n\n\
             def fast\n    @param x Int\n    @return Int\ndo\n  sleep(10)\n  x + 2\nend\n\n\
             def main\n    @return Int\ndo\n  let base = 10\n  let a, b = all(slow(base), fast(base))\n  a + b\nend\n",
        );
        let spec = extract_minimal_all_main(&ast, None).expect("async emitter spec");

        assert_eq!(spec.frame.resume_state.global, 0);
        assert_eq!(spec.frame.root_wake.global, 1);
        assert_eq!(spec.frame.locals.len(), 3);
        assert_eq!(spec.frame.post_wait_locals.len(), 2);
        assert_eq!(spec.frame.value_slot_count(), 5);
        assert_eq!(spec.frame.task_slot_count(), 4);
        assert_eq!(spec.root.delay_ms, 50.0);
        assert!(!spec.root.complete_immediately);
        assert!(spec.root.error.is_none());
        assert_eq!(spec.children.len(), 2);
        assert_eq!(spec.frame.pending_tasks.len(), 2);
        assert_eq!(spec.task_records.len(), 2);
        assert_eq!(spec.task_records[0].slot, spec.frame.pending_tasks[0]);
        assert_eq!(spec.task_records[0].child, spec.children[0]);
        assert_eq!(spec.frame.pending_tasks[0].task_id, 2);
        assert_eq!(spec.frame.pending_tasks[0].state_global, 8);
        assert_eq!(spec.frame.pending_tasks[0].wake_global, 10);
        assert_eq!(spec.frame.pending_tasks[0].result_local_idx, 3);
        assert_eq!(spec.frame.pending_tasks[1].task_id, 3);
        assert_eq!(spec.frame.pending_tasks[1].state_global, 9);
        assert_eq!(spec.frame.pending_tasks[1].wake_global, 11);
        assert_eq!(spec.frame.pending_tasks[1].result_local_idx, 4);
        assert_eq!(spec.heap_global(), 12);
        assert_eq!(
            spec.children[0],
            AsyncChildTaskSpec {
                delay_ms: 50.0,
                error: None,
                result_local_idx: 3,
                local_init_start: 3,
                local_init_count: 1,
            }
        );
        assert_eq!(
            spec.children[1],
            AsyncChildTaskSpec {
                delay_ms: 10.0,
                error: None,
                result_local_idx: 4,
                local_init_start: 4,
                local_init_count: 1,
            }
        );
        assert_eq!(spec.handlers, AsyncHandlerSpec::default());
    }

    #[test]
    fn extracts_minimal_nowait_child_without_waiting_for_result() {
        let ast = parse(
            "def child\n    @param x Int\n    @return Int\ndo\n  sleep(50)\n  x + 1\nend\n\n\
             def main\n    @return Int\ndo\n  let base = 10\n  nowait child(base)\n  base + 1\nend\n",
        );
        let spec = extract_minimal_nowait_main(&ast, None).expect("minimal nowait");
        assert_eq!(spec.frame.locals.len(), 2);
        assert_eq!(spec.frame.locals[0].name, "base");
        assert_eq!(spec.frame.locals[1].name, "child.x");
        assert_eq!(
            spec.frame.locals[1].value,
            ResultExpr::Int(IntExpr::Local(0))
        );
        assert_eq!(spec.child_delays_ms(), vec![50.0]);
        assert!(spec.root.complete_immediately);
        assert_eq!(
            spec.frame.post_wait_locals[0].value,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(1)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
        assert_eq!(
            spec.root.result,
            ResultExpr::Int(IntExpr::Binary {
                op: IntBinaryOp::Add,
                left: Box::new(IntExpr::Local(0)),
                right: Box::new(IntExpr::Literal(1)),
            })
        );
    }

    #[test]
    fn extracts_minimal_nowait_with_private_child_and_test_block() {
        let ast = parse(
            "def main\n    @return Int\ndo\n  let base = 10\n  nowait child(base)\n  base + 1\nend\n\n\
private:\n\n\
# Returns the argument after waiting.\n\
def child\n    @param x Int\n    @return Int\ndo\n  sleep(1)\n  x + 1\nend\n\n\
test child\n  it 'returns after waiting'\n    assert.equals(child(2), 3)\n  end\nend\n",
        );
        let spec = extract_minimal_nowait_main(&ast, None).expect("private child fixture shape");
        assert!(spec.root.complete_immediately);
        assert_eq!(spec.child_delays_ms(), vec![1.0]);
    }

    #[test]
    fn extracts_try_catch_finally_after_wait_throw() {
        // `finally` body is a single terminal integer expression. The
        // spec captures it as `finally_expr` and the catch result
        // becomes `catch_result + finally_expr` at emit time.
        let ast = parse(
            "def main\n    @return Int\ndo\n  try\n    sleep(1)\n    throw 7\n  catch e\n    100\n  finally\n    5\n  end\nend\n",
        );
        let spec = extract_minimal_try_main(&ast, None).expect("try/catch/finally after wait");
        assert_eq!(spec.root.error, Some(ThrowValue::IntLiteral(7)));
        assert_eq!(
            spec.handlers.catch_result,
            Some(ResultExpr::Int(IntExpr::Literal(100)))
        );
        assert_eq!(
            spec.handlers.finally_expr,
            Some(ResultExpr::Int(IntExpr::Literal(5)))
        );
    }

    #[test]
    fn extracts_try_catch_finally_after_successful_wait() {
        // No throw — finally should still be recorded so the success
        // result is `try_result + finally_expr` at emit time.
        let ast = parse(
            "def main\n    @return Int\ndo\n  try\n    sleep(1)\n    42\n  catch e\n    100\n  finally\n    5\n  end\nend\n",
        );
        let spec = extract_minimal_try_main(&ast, None).expect("try/catch/finally success path");
        assert!(spec.root.error.is_none());
        assert_eq!(spec.root.result, ResultExpr::Int(IntExpr::Literal(42)));
        assert_eq!(
            spec.handlers.catch_result,
            Some(ResultExpr::Int(IntExpr::Literal(100)))
        );
        assert_eq!(
            spec.handlers.finally_expr,
            Some(ResultExpr::Int(IntExpr::Literal(5)))
        );
    }

    #[test]
    fn extracts_try_catch_finally_after_auto_wait_throw() {
        // The catch is in main; the child frame still suspends and
        // the finally should still be recorded.
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  throw 9\nend\n\n\
             def main\n    @return Int\ndo\n  try\n    let x = child()\n    x + 1\n  catch e\n    100\n  finally\n    5\n  end\nend\n",
        );
        let spec = extract_minimal_try_main(&ast, None).expect("try/catch/finally auto-wait");
        assert_eq!(spec.root.error, Some(ThrowValue::IntLiteral(9)));
        assert_eq!(
            spec.handlers.catch_result,
            Some(ResultExpr::Int(IntExpr::Literal(100)))
        );
        assert_eq!(
            spec.handlers.finally_expr,
            Some(ResultExpr::Int(IntExpr::Literal(5)))
        );
    }

    #[test]
    fn extracts_try_catch_finally_after_all_throw() {
        // A failing `all` child wakes the waiting parent; the catch
        // runs in main and the finally must still be recorded.
        let ast = parse(
            "def slow\n    @return Int\ndo\n  sleep(50)\n  1\nend\n\n\
             def fast\n    @return Int\ndo\n  sleep(10)\n  throw 9\nend\n\n\
             def main\n    @return Int\ndo\n  try\n    let a, b = all(slow(), fast())\n    a + b\n  catch e\n    100\n  finally\n    5\n  end\nend\n",
        );
        let spec = extract_minimal_try_main(&ast, None).expect("try/catch/finally all");
        assert_eq!(
            spec.child_errors(),
            vec![None, Some(ThrowValue::IntLiteral(9))]
        );
        assert_eq!(
            spec.handlers.catch_result,
            Some(ResultExpr::Int(IntExpr::Literal(100)))
        );
        assert_eq!(
            spec.handlers.finally_expr,
            Some(ResultExpr::Int(IntExpr::Literal(5)))
        );
    }

    #[test]
    fn extracts_finally_with_let_then_terminal() {
        // A multi-statement finally is supported when it is zero or
        // more lets followed by a terminal expression.
        let ast = parse(
            "def main\n    @return Int\ndo\n  try\n    sleep(1)\n    1\n  catch e\n    2\n  finally\n    let x = 1\n    x\n  end\nend\n",
        );
        let spec = extract_minimal_try_main(&ast, None).expect("let-plus-terminal finally");
        assert_eq!(spec.handlers.finally_local_init_count, 1);
        assert_eq!(
            spec.handlers.finally_expr,
            Some(ResultExpr::Int(IntExpr::Local(1)))
        );
    }

    #[test]
    fn rejects_finally_with_throw() {
        // A re-throw in finally is not in the narrow shape.
        let ast = parse(
            "def main\n    @return Int\ndo\n  try\n    sleep(1)\n    1\n  catch e\n    2\n  finally\n    throw 7\n  end\nend\n",
        );
        let spec = extract_minimal_try_main(&ast, None);
        assert!(spec.is_none(), "throw in finally is not narrow");
    }

    #[test]
    fn extracts_error_throw_after_wait() {
        // `throw Error('msg')` after the suspension point. The spec
        // captures the message in `root_error` so the build step
        // can bake the dict into the data section.
        let ast = parse("def main\n    @return Int\ndo\n  sleep(1)\n  throw Error('boom')\nend\n");
        let error_messages = collect_unique_error_messages(&ast);
        let error_message = error_messages.first().cloned();
        assert_eq!(error_message.as_deref(), Some("boom"));
        let spec = extract_minimal_wait_main(&ast, error_message.as_deref())
            .expect("minimal wait with error throw");
        assert_eq!(
            spec.root.error,
            Some(ThrowValue::ErrorDict("boom".to_string()))
        );
    }

    #[test]
    fn extracts_error_throw_in_child() {
        // The child throws `Error('boom')`. The parent auto-wait
        // routes the error to its catch binding.
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  throw Error('boom')\nend\n\n\
             def main\n    @return Int\ndo\n  let x = child()\n  x + 1\nend\n",
        );
        let error_messages = collect_unique_error_messages(&ast);
        let error_message = error_messages.first().cloned();
        assert_eq!(error_message.as_deref(), Some("boom"));
        let spec = extract_minimal_auto_await_main(&ast, error_message.as_deref())
            .expect("auto-wait with error throw");
        assert_eq!(
            spec.root.error,
            Some(ThrowValue::ErrorDict("boom".to_string()))
        );
    }

    #[test]
    fn extracts_catch_body_with_error_message() {
        // The catch body reads `e.message` — a member access on
        // the catch binding. The spec records this as a
        // `StringExpr::ErrorMessage` in the catch result.
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  throw Error('boom')\nend\n\n\
             def main\n    @return String\ndo\n  try\n    let x = child()\n    'bad'\n  catch err\n    err.message\n  end\nend\n",
        );
        let error_messages = collect_unique_error_messages(&ast);
        let error_message = error_messages.first().cloned();
        assert_eq!(error_message.as_deref(), Some("boom"));
        let spec = extract_minimal_try_main(&ast, error_message.as_deref())
            .expect("try/catch with e.message");
        assert_eq!(
            spec.handlers.catch_result,
            Some(ResultExpr::String(StringExpr::ErrorMessage))
        );
    }

    #[test]
    fn extracts_catch_body_with_if_else_on_string_compare() {
        // The catch body is `if e.message == 'boom' { 'a' } else { 'b' }`.
        // The spec records the catch result as an `IfElse` whose
        // condition is a `StringEq` IntExpr and whose branches
        // are string literals.
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  throw Error('boom')\nend\n\n\
             def main\n    @return String\ndo\n  try\n    let x = child()\n    'unreached'\n  catch err\n    if err.message == 'boom'\n      'matched'\n    else\n      'other'\n    end\n  end\nend\n",
        );
        let error_messages = collect_unique_error_messages(&ast);
        let error_message = error_messages.first().cloned();
        let spec = extract_minimal_try_main(&ast, error_message.as_deref())
            .expect("try/catch with if/else on e.message");
        // The catch result should be an IfElse wrapping two
        // StringExpr::Literal branches and a StringEq condition.
        let if_else = match spec.handlers.catch_result {
            Some(ResultExpr::IfElse { .. }) => true,
            _ => false,
        };
        assert!(
            if_else,
            "expected IfElse catch result, got {:?}",
            spec.handlers.catch_result
        );
    }

    #[test]
    fn collects_multiple_distinct_error_messages() {
        // Multiple different `throw Error('...')` messages in the
        // same program are accepted. The data section bakes one
        // dict per unique message and the throw site picks the
        // matching one; `e.message` does a runtime field read.
        let ast = parse(
            "def a\n    @return Int\ndo\n  sleep(1)\n  throw Error('a-msg')\nend\n\n\
             def b\n    @return Int\ndo\n  sleep(1)\n  throw Error('b-msg')\nend\n\n\
             def main\n    @return Int\ndo\n  try\n    let x = a()\n    1\n  catch e\n    2\n  end\nend\n",
        );
        let error_messages = collect_unique_error_messages(&ast);
        assert_eq!(
            error_messages,
            vec!["a-msg".to_string(), "b-msg".to_string()]
        );
    }

    #[test]
    fn accepts_duplicate_error_message() {
        // The same message in two throws is fine — the narrow
        // path bakes one dict and both throws reference it.
        let ast = parse(
            "def a\n    @return Int\ndo\n  sleep(1)\n  throw Error('same')\nend\n\n\
             def b\n    @return Int\ndo\n  sleep(1)\n  throw Error('same')\nend\n\n\
             def main\n    @return Int\ndo\n  try\n    let x = a()\n    1\n  catch e\n    2\n  end\nend\n",
        );
        let error_messages = collect_unique_error_messages(&ast);
        let error_message = error_messages.first().cloned();
        assert_eq!(error_message.as_deref(), Some("same"));
    }
}
