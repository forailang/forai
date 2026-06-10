//! Real async engine — the guest-side scheduler (R1) and the layout the
//! resumable-function lowering (R2+) targets.
//!
//! The WASM realization of the `async_runtime` model: a dynamic, heap-backed
//! task table + ready queue + a `__fai_poll` loop that runs ready tasks by
//! calling their *resume function* (`() -> ()`) through the function table.
//! Each resume function reads its own task id from the `current` global, reads
//! and advances its `resume_state` in the task record, and at a suspension
//! point parks itself (`sleep`) and returns; the poll loop re-enters it after a
//! wakeup. Side effects therefore happen at real execution time.
//!
//! Host ABI (unchanged): `_start_async() -> i32`, `__fai_poll() -> i32`
//! (`2` = idle/root-complete, `3` = root-failed, else still working),
//! `__fai_resume_task(id) -> i32`, `__fai_task_result(id) -> i64`. The host
//! only provides wakeups (`now_ms`); the guest owns task state.
//!
//! The emitters are index-parameterized via [`SchedLayout`] so the same code
//! serves both the self-contained test module here and the production module
//! assembled in `direct.rs` (where the scheduler sits after the `RT_*`
//! helpers and the task table is allocated from the real heap).

use wasm_encoder::{BlockType, Function, Instruction, MemArg, ValType};

// ─── Task record layout (bytes, in linear memory) ───────────────────
pub const REC_SIZE: i32 = 56;
// i32: task status (READY/RUNNING/WAITING/COMPLETE/FAILED). Public def below.
const O_RESUME: u64 = 4; // i32: function-table index of the resume fn
/// i32: frame pointer (locals live across suspension). Public so the
/// resume lowering can read the current task's frame.
pub const O_FRAME: u64 = 8;
const O_JOIN: u64 = 12; // i32: outstanding child joins before resume
pub const O_RESULT: u64 = 16; // i64: NaN-boxed result // i64: NaN-boxed result
/// i64: NaN-boxed error of a failed task. Public so the resume lowering can
/// read a failed child's error to propagate it.
pub const O_ERROR: u64 = 24;
/// f64: timer deadline (-1.0 = not a timer wait). Public so the resume lowering
/// can park a task on a host op (e.g. a `remoteCall` fetch) with no timer.
pub const O_WAKE: u64 = 32;
pub const O_NEXT: u64 = 40; // i32: ready-queue link / free-list link (-1 = none)
const O_WAITER: u64 = 44; // i32: single parent waiter (-1 = none)
/// i32: resume-state label for the state machine. Public so the
/// direct-builder resume lowering can read/write it on the current task.
pub const O_RSTATE: u64 = 48;
/// i32: task status. Public so the resume lowering can detect a failed child.
pub const O_STATUS: u64 = 0;
/// i32: byte size of the task's frame heap block, recorded at spawn so
/// `complete`/`fail` can `rt_free` the frame (its locals are dead once the task
/// finishes; the result lives in `O_RESULT`). 0 = unset → frame not reclaimed
/// (sound: a missed spawn site leaks rather than freeing a wrong size). Sits in
/// the spare i32 at offset 52 (`O_RSTATE` is the last used field at 48).
pub const O_FRAME_SIZE: u64 = 52;

const ST_READY: i32 = 0;
const ST_RUNNING: i32 = 1;
/// Task is parked (awaiting a child, a timer, or a host op). Public so the
/// resume lowering can park a task on a `remoteCall`.
pub const ST_WAITING: i32 = 2;
pub const ST_COMPLETE: i32 = 3;
/// Task status value for a failed task. Public for the resume lowering.
pub const ST_FAILED: i32 = 4;
/// Task slot has been returned to the free list. Distinct from the terminal
/// states so a second free of the same slot is a no-op (idempotent free): a
/// slot must never be pushed onto `g_free_head` twice, or `spawn` would hand the
/// same slot to two live tasks (e.g. a parent and its own child → self-await →
/// the scheduler re-readies it forever). Freeing only ever acts on a slot whose
/// status is COMPLETE/FAILED; a freed (or live/reused) slot is skipped.
pub const ST_FREED: i32 = 5;

/// Resolved function/global indices for the scheduler helpers. Both the
/// test module and the production assembler fill this in for their own
/// layout and pass it to the emitters.
#[derive(Debug, Clone, Copy)]
pub struct SchedLayout {
    // function indices
    pub now_ms: u32,
    /// Allocator used to reserve the task table: `(size_bytes:i32) -> addr:i32`.
    pub alloc: u32,
    /// `rt_free` `(ptr:i32, size:i32) -> ()`: returns a heap block to the free
    /// list. `complete`/`fail` use it to reclaim the finished task's frame.
    pub free: u32,
    pub ready_push: u32,
    pub ready_pop: u32,
    pub spawn: u32,
    pub complete: u32,
    pub fail: u32,
    pub sleep: u32,
    pub notify: u32,
    pub poll: u32,
    pub resume_task: u32,
    pub task_result: u32,
    /// `(parent:i32, child:i32) -> ()`: register `parent` as `child`'s
    /// waiter, bump `parent.join_remaining`, and park `parent` (no
    /// timer). When `child` completes, `notify` wakes `parent` once its
    /// join count hits zero. Backs auto-await and `all`.
    pub await_fn: u32,
    /// `() -> ()` type index used for the resume-fn `call_indirect`.
    pub resume_type: u32,
    // global indices
    pub g_count: u32,
    pub g_head: u32,
    pub g_tail: u32,
    pub g_root: u32,
    pub g_current: u32,
    pub g_table_base: u32,
    /// Count of not-yet-finished tasks: `spawn` increments, `complete`/`fail`
    /// decrement. `poll` reports idle (terminal) only when this reaches 0, so
    /// a `nowait` task that outlives `main` still runs to completion.
    pub g_live: u32,
    /// Head of the free-slot list (a completed task whose result has been
    /// consumed), or -1 if empty. Freed records are chained through their
    /// `O_NEXT` field; `spawn` pops from here before bumping `g_count`. This
    /// keeps the task table bounded by *peak concurrency* rather than the
    /// cumulative number of tasks ever spawned (re-renders spawn a task per
    /// closure invocation, so without reclamation the table grows unbounded and
    /// overflows its allocation into live heap).
    pub g_free_head: u32,
    /// Function-table slot of `main`'s resume function.
    pub main_resume_table_idx: i32,
    /// Maximum number of tasks (v1: fixed-capacity bump, no reclamation).
    pub capacity: i32,
    /// Byte size of the root task's (`main`'s) frame, allocated at start.
    pub root_frame_size: i32,
    /// Optional `<__module_init__>` wasm fn index — runs module-level `var`
    /// initializers once before `main` is spawned. `None` if there are none.
    pub module_init: Option<u32>,
    /// Optional `host_set_timer(task_id, ms)` import index. When set (browser
    /// targets), `sleep` delegates the wakeup to the host instead of the native
    /// busy-poll: the task parks with `O_WAKE < 0` (poll won't promote it) and
    /// the host calls `__fai_resume_task` after `ms`. `None` → native busy-poll.
    pub set_timer: Option<u32>,
}

/// Number of scheduler helper functions emitted by [`emit_scheduler_functions`],
/// in this order: ready_push, ready_pop, spawn, complete, fail, sleep,
/// notify_waiter, poll, start_async, resume_task, task_result, await,
/// drive_closure.
pub const SCHED_FN_COUNT: u32 = 13;

fn ma(offset: u64) -> MemArg {
    MemArg {
        offset,
        align: 0,
        memory_index: 0,
    }
}

/// Emit `addr = table_base + id*REC_SIZE` for an id sitting in `local`.
fn rec_addr_local(f: &mut Function, l: &SchedLayout, local: u32) {
    f.instruction(&Instruction::GlobalGet(l.g_table_base));
    f.instruction(&Instruction::LocalGet(local));
    f.instruction(&Instruction::I32Const(REC_SIZE));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
}

/// Emit `addr = table_base + id*REC_SIZE` for an id sitting in `global`.
fn rec_addr_global(f: &mut Function, l: &SchedLayout, global: u32) {
    f.instruction(&Instruction::GlobalGet(l.g_table_base));
    f.instruction(&Instruction::GlobalGet(global));
    f.instruction(&Instruction::I32Const(REC_SIZE));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
}

fn emit_ready_push(l: &SchedLayout) -> Function {
    // param: id = local 0
    let mut f = Function::new([]);
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::I32Store(ma(O_NEXT)));
    f.instruction(&Instruction::GlobalGet(l.g_tail));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::GlobalSet(l.g_head));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::GlobalSet(l.g_tail));
    f.instruction(&Instruction::Else);
    rec_addr_global(&mut f, l, l.g_tail);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Store(ma(O_NEXT)));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::GlobalSet(l.g_tail));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

fn emit_ready_pop(l: &SchedLayout) -> Function {
    // -> i32; local 0 = id
    let mut f = Function::new([(1, ValType::I32)]);
    f.instruction(&Instruction::GlobalGet(l.g_head));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::GlobalGet(l.g_head));
    f.instruction(&Instruction::LocalSet(0));
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::I32Load(ma(O_NEXT)));
    f.instruction(&Instruction::GlobalSet(l.g_head));
    f.instruction(&Instruction::GlobalGet(l.g_head));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::GlobalSet(l.g_tail));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

fn emit_spawn(l: &SchedLayout) -> Function {
    // params: resume_fn = 0, frame_ptr = 1; locals: id = 2, addr = 3
    let mut f = Function::new([(2, ValType::I32)]);
    // Pick a slot: reuse the head of the free list (a completed task whose
    // result was consumed) if any, else bump `g_count`. Reclamation keeps the
    // table bounded by peak concurrency rather than cumulative spawns.
    f.instruction(&Instruction::GlobalGet(l.g_free_head));
    f.instruction(&Instruction::LocalTee(2)); // id = g_free_head (tentative)
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(BlockType::Empty));
    // free-list pop: g_free_head = freed[id].next  (id already in local 2)
    rec_addr_local(&mut f, l, 2);
    f.instruction(&Instruction::I32Load(ma(O_NEXT)));
    f.instruction(&Instruction::GlobalSet(l.g_free_head));
    f.instruction(&Instruction::Else);
    // bump: id = g_count; trap on overflow rather than scribbling past the
    // table into live heap; then g_count++.
    f.instruction(&Instruction::GlobalGet(l.g_count));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(l.capacity));
    f.instruction(&Instruction::I32GeS);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::GlobalGet(l.g_count));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalSet(l.g_count));
    f.instruction(&Instruction::End);
    rec_addr_local(&mut f, l, 2);
    f.instruction(&Instruction::LocalSet(3));
    let set_i32 = |f: &mut Function, off: u64, v: Instruction| {
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&v);
        f.instruction(&Instruction::I32Store(ma(off)));
    };
    set_i32(&mut f, O_STATUS, Instruction::I32Const(ST_READY));
    set_i32(&mut f, O_RESUME, Instruction::LocalGet(0));
    set_i32(&mut f, O_FRAME, Instruction::LocalGet(1));
    set_i32(&mut f, O_JOIN, Instruction::I32Const(0));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I64Const(crate::runtime::VAL_VOID));
    f.instruction(&Instruction::I64Store(ma(O_RESULT)));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I64Const(crate::runtime::VAL_VOID));
    f.instruction(&Instruction::I64Store(ma(O_ERROR)));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::F64Const(-1.0));
    f.instruction(&Instruction::F64Store(ma(O_WAKE)));
    set_i32(&mut f, O_NEXT, Instruction::I32Const(-1));
    set_i32(&mut f, O_WAITER, Instruction::I32Const(-1));
    set_i32(&mut f, O_RSTATE, Instruction::I32Const(0));
    // Reset the frame-size field; the spawn SITE overwrites it with the real
    // size. If a site forgets, this 0 means "don't free" — a leak, not a
    // corruption (vs. a stale size left in a recycled slot).
    set_i32(&mut f, O_FRAME_SIZE, Instruction::I32Const(0));
    // live++ (one more task not yet finished)
    f.instruction(&Instruction::GlobalGet(l.g_live));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalSet(l.g_live));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::Call(l.ready_push));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::End);
    f
}

fn emit_complete_or_fail(l: &SchedLayout, status: i32, value_off: u64) -> Function {
    // params: id = 0, value = 1 (i64); local addr = 2
    let mut f = Function::new([(1, ValType::I32)]);
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(status));
    f.instruction(&Instruction::I32Store(ma(O_STATUS)));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Store(ma(value_off)));
    // Reclaim the frame heap block — the task is finished, so its locals are
    // dead (the result is in O_RESULT, not the frame) and it will never resume.
    // Guard on a nonzero recorded size: 0 means the spawn site didn't record
    // one, so leave it (sound). `rt_free`'s exact-fit reuse means the next task
    // of the same shape reuses this block.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(ma(O_FRAME_SIZE)));
    f.instruction(&Instruction::If(BlockType::Empty)); // size != 0
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(ma(O_FRAME))); // frame ptr
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(ma(O_FRAME_SIZE))); // size
    f.instruction(&Instruction::Call(l.free));
    f.instruction(&Instruction::End);
    // live-- (this task is finished)
    f.instruction(&Instruction::GlobalGet(l.g_live));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::GlobalSet(l.g_live));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(l.notify));
    // Reclaim a detached task (`O_WAITER == -1`: no scheduler waiter, e.g. a
    // `nowait` fire-and-forget, whose result no one reads). Tasks with a real
    // waiter (>= 0) are freed by that waiter after it consumes the result; the
    // host-driven sentinel (-2: root / `__fai_drive_closure`) is freed by the
    // host after it reads `task_result`. Without this, every detached task
    // permanently consumes a table slot and the table grows unbounded.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(ma(O_WAITER)));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(BlockType::Empty));
    // task[id].next = g_free_head; g_free_head = id
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::GlobalGet(l.g_free_head));
    f.instruction(&Instruction::I32Store(ma(O_NEXT)));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::GlobalSet(l.g_free_head));
    // Mark freed so a stray second free of this slot is a no-op (idempotent).
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(ST_FREED));
    f.instruction(&Instruction::I32Store(ma(O_STATUS)));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

fn emit_sleep(l: &SchedLayout) -> Function {
    // params: id = 0, ms = 1 (f64); local addr = 2
    let mut f = Function::new([(1, ValType::I32)]);
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(ST_WAITING));
    f.instruction(&Instruction::I32Store(ma(O_STATUS)));
    if let Some(set_timer) = l.set_timer {
        // Browser: park with O_WAKE = -1 (poll skips it) and let the host
        // arrange the wakeup via host_set_timer(task_id, ms).
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::F64Const(-1.0));
        f.instruction(&Instruction::F64Store(ma(O_WAKE)));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32TruncF64S);
        f.instruction(&Instruction::Call(set_timer));
    } else {
        // Native: record the wake time; poll promotes by busy-checking now_ms.
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(l.now_ms));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::F64Add);
        f.instruction(&Instruction::F64Store(ma(O_WAKE)));
    }
    f.instruction(&Instruction::End);
    f
}

fn emit_notify_waiter(l: &SchedLayout) -> Function {
    // param: id = 0; locals: w = 1, jr = 2
    let mut f = Function::new([(2, ValType::I32)]);
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::I32Load(ma(O_WAITER)));
    f.instruction(&Instruction::LocalSet(1));
    f.instruction(&Instruction::LocalGet(1));
    // A real scheduler waiter is a task id (>= 0). Negative sentinels mean "no
    // scheduler waiter": -1 = detached (freed at completion), -2 = host-driven
    // (the root / a `__fai_drive_closure` task, whose result the host reads).
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32GeS);
    f.instruction(&Instruction::If(BlockType::Empty));
    // First-completed error: if this child failed and the waiter hasn't yet
    // recorded a child error, store this child's error in the waiter's error
    // slot. (`all` resumes once all children finish; this preserves the
    // error of whichever failed first in completion order.)
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::I32Load(ma(O_STATUS)));
    f.instruction(&Instruction::I32Const(ST_FAILED));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(BlockType::Empty));
    rec_addr_local(&mut f, l, 1);
    f.instruction(&Instruction::I64Load(ma(O_ERROR)));
    f.instruction(&Instruction::I64Const(crate::runtime::VAL_VOID));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::If(BlockType::Empty));
    rec_addr_local(&mut f, l, 1);
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::I64Load(ma(O_RESULT)));
    f.instruction(&Instruction::I64Store(ma(O_ERROR)));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    rec_addr_local(&mut f, l, 1);
    f.instruction(&Instruction::I32Load(ma(O_JOIN)));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32GtS);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(2));
    rec_addr_local(&mut f, l, 1);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Store(ma(O_JOIN)));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(BlockType::Empty));
    rec_addr_local(&mut f, l, 1);
    f.instruction(&Instruction::I32Const(ST_READY));
    f.instruction(&Instruction::I32Store(ma(O_STATUS)));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(l.ready_push));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

fn emit_poll(l: &SchedLayout) -> Function {
    // -> i32; locals: i = 0, id = 1
    let mut f = Function::new([(2, ValType::I32)]);

    // timer promotion: for i in 0..count
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(0));
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::GlobalGet(l.g_count));
    f.instruction(&Instruction::I32GeS);
    f.instruction(&Instruction::BrIf(1));
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::I32Load(ma(O_STATUS)));
    f.instruction(&Instruction::I32Const(ST_WAITING));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(BlockType::Empty));
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::F64Load(ma(O_WAKE)));
    f.instruction(&Instruction::F64Const(0.0));
    f.instruction(&Instruction::F64Ge);
    f.instruction(&Instruction::If(BlockType::Empty));
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::F64Load(ma(O_WAKE)));
    f.instruction(&Instruction::Call(l.now_ms));
    f.instruction(&Instruction::F64Le);
    f.instruction(&Instruction::If(BlockType::Empty));
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::I32Const(ST_READY));
    f.instruction(&Instruction::I32Store(ma(O_STATUS)));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(l.ready_push));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(0));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // run ready queue to quiescence
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::Call(l.ready_pop));
    f.instruction(&Instruction::LocalSet(1));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::BrIf(1));
    rec_addr_local(&mut f, l, 1);
    f.instruction(&Instruction::I32Const(ST_RUNNING));
    f.instruction(&Instruction::I32Store(ma(O_STATUS)));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::GlobalSet(l.g_current));
    rec_addr_local(&mut f, l, 1);
    f.instruction(&Instruction::I32Load(ma(O_RESUME)));
    f.instruction(&Instruction::CallIndirect {
        type_index: l.resume_type,
        table_index: 0,
    });
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Terminal status. A failed root reports failure (3) immediately. Else
    // we drain until idle: still-live tasks → working (1); none → done (2).
    // This keeps a `nowait` task that outlives `main` running to completion;
    // the root's result is still what `__fai_task_result` returns.
    f.instruction(&Instruction::GlobalGet(l.g_root));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    rec_addr_global(&mut f, l, l.g_root);
    f.instruction(&Instruction::I32Load(ma(O_STATUS)));
    f.instruction(&Instruction::I32Const(ST_FAILED));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    f.instruction(&Instruction::I32Const(3));
    f.instruction(&Instruction::Else);
    // live != 0 ? working(1) : idle/complete(2)
    f.instruction(&Instruction::GlobalGet(l.g_live));
    f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::I32Const(2));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::GlobalGet(l.g_live));
    f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::I32Const(2));
    f.instruction(&Instruction::End); // close inner live-check (root-present else)
    f.instruction(&Instruction::End); // close root-present If
    f.instruction(&Instruction::End); // function body
    f
}

fn emit_start_async(l: &SchedLayout) -> Function {
    // -> i32: reserve the task table and the root frame, spawn main, poll.
    let mut f = Function::new([(1, ValType::I32)]); // local 0 = root frame
    f.instruction(&Instruction::I32Const(l.capacity * REC_SIZE));
    f.instruction(&Instruction::Call(l.alloc));
    f.instruction(&Instruction::GlobalSet(l.g_table_base));
    f.instruction(&Instruction::I32Const(l.root_frame_size));
    f.instruction(&Instruction::Call(l.alloc));
    f.instruction(&Instruction::LocalSet(0));
    // Zero the root frame (plan 115): unwritten slots must read 0 so async
    // reclamation's RT_RELEASE at completion is a no-op rather than a free of a
    // stale pointer left in a recycled block.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32Const(l.root_frame_size));
    f.instruction(&Instruction::MemoryFill(0));
    // Run module-level `var` initializers once, before `main` spawns. The
    // init fn is a sync `() -> i64` (void); discard its result.
    if let Some(mi) = l.module_init {
        f.instruction(&Instruction::Call(mi));
        f.instruction(&Instruction::Drop);
    }
    f.instruction(&Instruction::I32Const(l.main_resume_table_idx));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(l.spawn));
    f.instruction(&Instruction::GlobalSet(l.g_root));
    // Record the root frame's size so completion reclaims it.
    rec_addr_global(&mut f, l, l.g_root);
    f.instruction(&Instruction::I32Const(l.root_frame_size));
    f.instruction(&Instruction::I32Store(ma(O_FRAME_SIZE)));
    // Mark the root host-driven (-2) so completion doesn't recycle its slot —
    // the host reads its result via `__fai_task_result(g_root)`.
    rec_addr_global(&mut f, l, l.g_root);
    f.instruction(&Instruction::I32Const(-2));
    f.instruction(&Instruction::I32Store(ma(O_WAITER)));
    f.instruction(&Instruction::Call(l.poll));
    f.instruction(&Instruction::End);
    f
}

fn emit_resume_task(l: &SchedLayout) -> Function {
    // param: id = 0; -> i32
    let mut f = Function::new([]);
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::I32Const(ST_READY));
    f.instruction(&Instruction::I32Store(ma(O_STATUS)));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(l.ready_push));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::End);
    f
}

fn emit_task_result(l: &SchedLayout) -> Function {
    // param: id = 0; -> i64
    let mut f = Function::new([]);
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::I64Load(ma(O_RESULT)));
    f.instruction(&Instruction::End);
    f
}

fn emit_await(l: &SchedLayout) -> Function {
    // params: parent = 0, child = 1; local: parent_addr = 2
    let mut f = Function::new([(1, ValType::I32)]);
    // child.waiter = parent
    rec_addr_local(&mut f, l, 1);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Store(ma(O_WAITER)));
    // parent_addr
    rec_addr_local(&mut f, l, 0);
    f.instruction(&Instruction::LocalSet(2));
    // parent.join += 1
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(ma(O_JOIN)));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store(ma(O_JOIN)));
    // parent.status = WAITING
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(ST_WAITING));
    f.instruction(&Instruction::I32Store(ma(O_STATUS)));
    // parent.wake = -1: this is a join-wait, not a timer wait. Without this, a
    // stale `O_WAKE` left by an earlier `sleep` on the same task would make
    // `poll` spuriously timer-promote it before its child completes.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::F64Const(-1.0));
    f.instruction(&Instruction::F64Store(ma(O_WAKE)));
    f.instruction(&Instruction::End);
    f
}

/// The 11 scheduler helper functions, in declaration order:
/// `ready_push, ready_pop, spawn, complete, fail, sleep, notify_waiter,
/// poll, start_async, resume_task, task_result`.
pub fn emit_scheduler_functions(l: &SchedLayout) -> Vec<Function> {
    vec![
        emit_ready_push(l),
        emit_ready_pop(l),
        emit_spawn(l),
        emit_complete_or_fail(l, ST_COMPLETE, O_RESULT),
        // Failed tasks store the error in the result slot too, so
        // `task_result` (and the host failure report) surface it.
        emit_complete_or_fail(l, ST_FAILED, O_RESULT),
        emit_sleep(l),
        emit_notify_waiter(l),
        emit_poll(l),
        emit_start_async(l),
        emit_resume_task(l),
        emit_task_result(l),
        emit_await(l),
        emit_drive_closure(l),
    ]
}

/// `__fai_drive_closure(closure_val: i64, arg: i64) -> i64`: the host-driver
/// entry. When the wasm runner needs to invoke a guest closure that is *async*
/// (a resume fn — its header carries `frame_size > 0`), it can't `call_indirect`
/// it like a sync `FaiFunc` (signatures differ: a resume fn is `()->()` driven
/// by the scheduler). Instead the host calls this: it spawns the closure as a
/// task — frame[0] = the closure's env pointer, frame[8] = the single argument
/// (route/event handlers take one) — drives `poll` until that task finishes, and
/// returns its result. `g_current` is saved/restored because this runs
/// re-entrantly: `main` is still live, parked inside the host's blocking
/// `server.listen`, and must remain the current task afterward.
fn emit_drive_closure(l: &SchedLayout) -> Function {
    // params: closure_val = 0 (i64), arg = 1 (i64)
    // locals: addr = 2, frame = 3, id = 4, saved = 5 (all i32)
    let mut f = Function::new([(4, ValType::I32)]);
    // saved = g_current
    f.instruction(&Instruction::GlobalGet(l.g_current));
    f.instruction(&Instruction::LocalSet(5));
    // addr = (closure_val & ADDR_MASK) as i32
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(0x0000_FFFF_FFFF_FFFF));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::LocalSet(2));
    // frame = alloc(closure.frame_size @ addr+12)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(ma(12)));
    f.instruction(&Instruction::Call(l.alloc));
    f.instruction(&Instruction::LocalSet(3));
    // Zero the fresh frame (plan 115): freed frame blocks are reused without
    // clearing, so an unwritten slot would hold a stale pointer that async
    // reclamation would double-free. Zeroing makes it read 0 (RT_RELEASE no-op).
    // env/arg are written over the leading zeros below.
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(ma(12)));
    f.instruction(&Instruction::MemoryFill(0));
    // frame[0] = env_ptr = addr + 16 (upvalues begin past the 16-byte header)
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store(ma(0)));
    // frame[8] = arg (first param sits past the env slot)
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Store(ma(8)));
    // id = spawn(table_idx @ addr+4, frame)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(ma(4)));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::Call(l.spawn));
    f.instruction(&Instruction::LocalSet(4));
    // Record the driven closure frame's size (closure header @ +12) for reclaim.
    rec_addr_local(&mut f, l, 4);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(ma(12)));
    f.instruction(&Instruction::I32Store(ma(O_FRAME_SIZE)));
    // Mark the driven task host-driven (-2) so its completion doesn't recycle
    // the slot — this driver reads `task_result` afterward and frees it itself.
    rec_addr_local(&mut f, l, 4);
    f.instruction(&Instruction::I32Const(-2));
    f.instruction(&Instruction::I32Store(ma(O_WAITER)));
    // drive: loop { poll(); if task[id] done break; (browser) if the ready
    // queue empties with the task still parked, break and RETURN — it's waiting
    // on a host op (a `remoteCall` fetch / a timer) that only resolves via the JS
    // event loop, so busy-polling would deadlock it. The host wakes it later via
    // `__fai_resume_task` + `__fai_poll`. On native (no event loop) we busy-poll,
    // since host ops there re-ready the task synchronously.
    let yield_when_stuck = l.set_timer.is_some();
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::Call(l.poll));
    f.instruction(&Instruction::Drop);
    rec_addr_local(&mut f, l, 4);
    f.instruction(&Instruction::I32Load(ma(O_STATUS)));
    f.instruction(&Instruction::I32Const(ST_COMPLETE));
    f.instruction(&Instruction::I32GeS);
    f.instruction(&Instruction::BrIf(1));
    if yield_when_stuck {
        f.instruction(&Instruction::GlobalGet(l.g_head));
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::BrIf(1));
    }
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    // g_current = saved
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::GlobalSet(l.g_current));
    // If the task finished, return its result and recycle its slot (this driver
    // is its sole consumer — no scheduler waiter). If it only *yielded* (browser
    // yield-when-stuck: parked on a host op, not yet complete), leave the slot
    // live — the host will resume it later — and return Void.
    rec_addr_local(&mut f, l, 4);
    f.instruction(&Instruction::I32Load(ma(O_STATUS)));
    f.instruction(&Instruction::I32Const(ST_COMPLETE));
    f.instruction(&Instruction::I32GeS);
    f.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::Call(l.task_result)); // result on stack
    // free id: task[id].next = g_free_head; g_free_head = id
    f.instruction(&Instruction::GlobalGet(l.g_table_base));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(REC_SIZE));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalGet(l.g_free_head));
    f.instruction(&Instruction::I32Store(ma(O_NEXT)));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::GlobalSet(l.g_free_head));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::I64Const(crate::runtime::VAL_VOID));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ─── Resume-body building blocks (used by the direct-builder lowering) ──

/// Emit `current_task.resume_state` load (an i32 on the stack).
pub fn emit_load_resume_state(f: &mut Function, l: &SchedLayout) {
    rec_addr_global(f, l, l.g_current);
    f.instruction(&Instruction::I32Load(ma(O_RSTATE)));
}

/// Emit `current_task.resume_state = state`.
pub fn emit_store_resume_state(f: &mut Function, l: &SchedLayout, state: i32) {
    rec_addr_global(f, l, l.g_current);
    f.instruction(&Instruction::I32Const(state));
    f.instruction(&Instruction::I32Store(ma(O_RSTATE)));
}

/// Emit `sleep(current, ms)` — park the current task on a timer.
pub fn emit_suspend_sleep(f: &mut Function, l: &SchedLayout, ms: f64) {
    f.instruction(&Instruction::GlobalGet(l.g_current));
    f.instruction(&Instruction::F64Const(ms));
    f.instruction(&Instruction::Call(l.sleep));
}

/// Emit `complete(current, <value pushed by `push_value`>)`.
pub fn emit_complete_current_with(
    f: &mut Function,
    l: &SchedLayout,
    push_value: impl FnOnce(&mut Function),
) {
    f.instruction(&Instruction::GlobalGet(l.g_current));
    push_value(f);
    f.instruction(&Instruction::Call(l.complete));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use wasm_encoder::{
        CodeSection, ConstExpr, ElementSection, Elements, EntityType, ExportKind, ExportSection,
        FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection, MemoryType,
        Module, RefType, TableSection, TableType, TypeSection,
    };
    use wasmtime::{Engine, Instance, Linker, Module as WtModule, Store};

    thread_local! {
        static CLOCK_MS: Cell<f64> = const { Cell::new(0.0) };
    }

    /// Build a self-contained scheduler module driven by hand-written
    /// resume bodies (table index 0 = `main`). One import (`now_ms`); a
    /// trivial `alloc` returns a fixed base so no real heap is needed.
    fn test_module(resume_bodies: Vec<Function>) -> (Vec<u8>, SchedLayout) {
        // Function indices: import now_ms = 0; then defined functions.
        // [1..=13] scheduler helpers (13 = drive_closure), 14 = alloc,
        // 15.. = resume bodies.
        let n = resume_bodies.len() as u32;
        let layout = SchedLayout {
            now_ms: 0,
            ready_push: 1,
            ready_pop: 2,
            spawn: 3,
            complete: 4,
            fail: 5,
            sleep: 6,
            notify: 7,
            poll: 8,
            // start_async = 9, resume_task = 10, task_result = 11
            resume_task: 10,
            task_result: 11,
            await_fn: 12,
            // 13 = drive_closure (emitted by emit_scheduler_functions before alloc)
            alloc: 14,
            // 15 = no-op free (test frames record size 0, so it's never called
            // at runtime — present only so `complete`'s `Call(free)` validates).
            free: 15,
            resume_type: 1, // () -> ()
            g_count: 0,
            g_head: 1,
            g_tail: 2,
            g_root: 3,
            g_current: 4,
            g_table_base: 5,
            g_live: 6,
            g_free_head: 7,
            main_resume_table_idx: 0,
            capacity: 64,
            root_frame_size: 16,
            module_init: None,
            set_timer: None,
        };

        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function(vec![], vec![ValType::F64]); // 0: now_ms
        types.ty().function(vec![], vec![]); // 1: resume () -> ()
        types.ty().function(vec![ValType::I32], vec![]); // 2: (i32) -> ()
        types.ty().function(vec![], vec![ValType::I32]); // 3: () -> i32
        types
            .ty()
            .function(vec![ValType::I32, ValType::I32], vec![ValType::I32]); // 4 spawn
        types
            .ty()
            .function(vec![ValType::I32, ValType::I64], vec![]); // 5 complete/fail
        types
            .ty()
            .function(vec![ValType::I32, ValType::F64], vec![]); // 6 sleep
        types.ty().function(vec![ValType::I32], vec![ValType::I32]); // 7 resume_task / alloc
        types.ty().function(vec![ValType::I32], vec![ValType::I64]); // 8 task_result
        types
            .ty()
            .function(vec![ValType::I32, ValType::I32], vec![]); // 9 await (i32,i32)->()
        types
            .ty()
            .function(vec![ValType::I64, ValType::I64], vec![ValType::I64]); // 10 drive_closure
        module.section(&types);

        let mut imports = ImportSection::new();
        imports.import("env", "now_ms", EntityType::Function(0));
        module.section(&imports);

        let mut funcs = FunctionSection::new();
        funcs.function(2); // ready_push (i32)->()
        funcs.function(3); // ready_pop ()->i32
        funcs.function(4); // spawn
        funcs.function(5); // complete
        funcs.function(5); // fail
        funcs.function(6); // sleep
        funcs.function(2); // notify
        funcs.function(3); // poll ()->i32
        funcs.function(3); // start_async ()->i32
        funcs.function(7); // resume_task (i32)->i32
        funcs.function(8); // task_result (i32)->i64
        funcs.function(9); // await (i32,i32)->()
        funcs.function(10); // drive_closure (i64,i64)->i64
        funcs.function(7); // alloc (i32)->i32
        funcs.function(9); // free (i32,i32)->() — no-op stub (type 9)
        for _ in &resume_bodies {
            funcs.function(1); // resume body ()->()
        }
        module.section(&funcs);

        let mut tables = TableSection::new();
        tables.table(TableType {
            element_type: RefType::FUNCREF,
            minimum: n as u64,
            maximum: Some(n as u64),
            table64: false,
            shared: false,
        });
        module.section(&tables);

        let mut mem = MemorySection::new();
        mem.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&mem);

        let mut globals = GlobalSection::new();
        let g = GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        };
        globals.global(g, &ConstExpr::i32_const(0)); // count
        globals.global(g, &ConstExpr::i32_const(-1)); // head
        globals.global(g, &ConstExpr::i32_const(-1)); // tail
        globals.global(g, &ConstExpr::i32_const(-1)); // root
        globals.global(g, &ConstExpr::i32_const(-1)); // current
        globals.global(g, &ConstExpr::i32_const(0)); // table_base
        globals.global(g, &ConstExpr::i32_const(0)); // live
        globals.global(g, &ConstExpr::i32_const(-1)); // free_head
        module.section(&globals);

        let mut exports = ExportSection::new();
        exports.export("_start_async", ExportKind::Func, 9);
        exports.export("__fai_poll", ExportKind::Func, 8);
        exports.export("__fai_resume_task", ExportKind::Func, 10);
        exports.export("__fai_task_result", ExportKind::Func, 11);
        exports.export("memory", ExportKind::Memory, 0);
        module.section(&exports);

        if n > 0 {
            let mut elements = ElementSection::new();
            let idxs: Vec<u32> = (0..n).map(|i| 16 + i).collect();
            elements.active(
                Some(0),
                &ConstExpr::i32_const(0),
                Elements::Functions(idxs.into()),
            );
            module.section(&elements);
        }

        let mut code = CodeSection::new();
        for func in emit_scheduler_functions(&layout) {
            code.function(&func);
        }
        // alloc: ignore size, return a fixed base (1024).
        let mut alloc = Function::new([]);
        alloc.instruction(&Instruction::I32Const(1024));
        alloc.instruction(&Instruction::End);
        code.function(&alloc);
        // free: no-op stub (i32,i32)->(). Never called in tests (frames record
        // size 0); exists so `complete`'s guarded `Call(free)` validates.
        let mut free = Function::new([]);
        free.instruction(&Instruction::End);
        code.function(&free);
        for body in &resume_bodies {
            code.function(body);
        }
        module.section(&code);

        (module.finish(), layout)
    }

    fn instantiate(wasm: &[u8]) -> (Store<()>, Instance) {
        let engine = Engine::default();
        let module = WtModule::new(&engine, wasm).expect("module should validate");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        linker
            .func_wrap("env", "now_ms", || -> f64 { CLOCK_MS.with(|c| c.get()) })
            .unwrap();
        let instance = linker.instantiate(&mut store, &module).expect("instantiate");
        (store, instance)
    }

    fn body_immediate(l: &SchedLayout, value: i64) -> Function {
        let mut f = Function::new([]);
        emit_complete_current_with(&mut f, l, |f| {
            f.instruction(&Instruction::I64Const(value));
        });
        f.instruction(&Instruction::End);
        f
    }

    fn body_sleep_then_complete(l: &SchedLayout, ms: f64, value: i64) -> Function {
        let mut f = Function::new([]);
        emit_load_resume_state(&mut f, l);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        emit_store_resume_state(&mut f, l, 1);
        emit_suspend_sleep(&mut f, l, ms);
        f.instruction(&Instruction::Else);
        emit_complete_current_with(&mut f, l, |f| {
            f.instruction(&Instruction::I64Const(value));
        });
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    #[test]
    fn scheduler_runs_a_task_to_completion_through_the_host_abi() {
        CLOCK_MS.with(|c| c.set(0.0));
        // Build twice: once to get the layout, once with the real body.
        let (_, layout) = test_module(vec![Function::new([])]);
        let wasm = test_module(vec![body_immediate(&layout, 123)]).0;
        let (mut store, inst) = instantiate(&wasm);
        let start = inst
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .unwrap();
        let result = inst
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .unwrap();
        assert_eq!(start.call(&mut store, ()).unwrap(), 2);
        assert_eq!(result.call(&mut store, 0).unwrap(), 123);
    }

    #[test]
    fn task_suspends_on_sleep_then_resumes_when_the_clock_passes() {
        CLOCK_MS.with(|c| c.set(0.0));
        let (_, layout) = test_module(vec![Function::new([])]);
        let wasm = test_module(vec![body_sleep_then_complete(&layout, 50.0, 456)]).0;
        let (mut store, inst) = instantiate(&wasm);
        let start = inst
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .unwrap();
        let poll = inst
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .unwrap();
        let result = inst
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .unwrap();
        assert_eq!(start.call(&mut store, ()).unwrap(), 1);
        // Not yet completed: result slot still holds VAL_VOID.
        assert_eq!(result.call(&mut store, 0).unwrap(), crate::runtime::VAL_VOID);
        CLOCK_MS.with(|c| c.set(20.0));
        assert_eq!(poll.call(&mut store, ()).unwrap(), 1);
        CLOCK_MS.with(|c| c.set(50.0));
        assert_eq!(poll.call(&mut store, ()).unwrap(), 2);
        assert_eq!(result.call(&mut store, 0).unwrap(), 456);
    }

    #[test]
    fn resume_task_wakes_a_suspended_task_without_the_clock() {
        CLOCK_MS.with(|c| c.set(0.0));
        let (_, layout) = test_module(vec![Function::new([])]);
        let wasm = test_module(vec![body_sleep_then_complete(&layout, 9999.0, 77)]).0;
        let (mut store, inst) = instantiate(&wasm);
        let start = inst
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .unwrap();
        let resume = inst
            .get_typed_func::<i32, i32>(&mut store, "__fai_resume_task")
            .unwrap();
        let poll = inst
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .unwrap();
        let result = inst
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .unwrap();
        assert_eq!(start.call(&mut store, ()).unwrap(), 1);
        assert_eq!(resume.call(&mut store, 0).unwrap(), 0);
        assert_eq!(poll.call(&mut store, ()).unwrap(), 2);
        assert_eq!(result.call(&mut store, 0).unwrap(), 77);
    }

    /// Parent (table 0, root id 0) spawns child (table 1, id 1), awaits
    /// it, and on resume completes with the child's result.
    fn body_await_child(l: &SchedLayout) -> Function {
        let mut f = Function::new([(1, ValType::I32)]); // local 0 = child id
        emit_load_resume_state(&mut f, l);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        // spawn(child_table=1, frame=0) -> child_id
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Call(l.spawn));
        f.instruction(&Instruction::LocalSet(0));
        // await(current, child_id)
        f.instruction(&Instruction::GlobalGet(l.g_current));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(l.await_fn));
        emit_store_resume_state(&mut f, l, 1);
        f.instruction(&Instruction::Else);
        // complete(current, task_result(child id 1))
        f.instruction(&Instruction::GlobalGet(l.g_current));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Call(l.task_result));
        f.instruction(&Instruction::Call(l.complete));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    fn body_child_const(l: &SchedLayout, value: i64) -> Function {
        let mut f = Function::new([]);
        emit_complete_current_with(&mut f, l, |f| {
            f.instruction(&Instruction::I64Const(value));
        });
        f.instruction(&Instruction::End);
        f
    }

    #[test]
    fn parent_awaits_child_and_reads_its_result() {
        CLOCK_MS.with(|c| c.set(0.0));
        let (_, layout) = test_module(vec![Function::new([]), Function::new([])]);
        let wasm = test_module(vec![body_await_child(&layout), body_child_const(&layout, 99)]).0;
        let (mut store, inst) = instantiate(&wasm);
        let start = inst
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .unwrap();
        let result = inst
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .unwrap();
        // Parent suspends on await; child runs and completes in the same
        // poll; parent resumes and completes with the child's result.
        assert_eq!(start.call(&mut store, ()).unwrap(), 2);
        assert_eq!(result.call(&mut store, 0).unwrap(), 99);
    }
}
