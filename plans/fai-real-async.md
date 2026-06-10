# Plan: Real Async for fai

Status: proposal, not yet implemented.

Build compiler/runtime-level async with explicit forai task scheduling. The
goal is not to bolt async onto one host target, but to make async a stable
language/runtime capability that behaves the same for CLI/server and browser
code writers.

## Goals

1. **Await by default at call sites.**

   ```fai
   let result = someAsyncFunc(a, b)
   ```

   If `someAsyncFunc` suspends internally, the caller waits for its final
   value. User code does not write `await`.

2. **One user model across targets.**

   Browser, CLI, and server may have different host wakeup sources, but
   forai code observes the same semantics.

3. **Start narrow, design for extension.**

   v0 supports only:

   - `wait(ms)` — suspend the current task for at least `ms` milliseconds.
   - `all(a(), b(), ...)` — run child tasks concurrently and resume with all
     results.

   The same machinery must later support HTTP/RPC, events, process IO,
   browser timers, and `nowait`.

4. **Stable foundation over quick target-specific wins.**

   Avoid making Wasmtime async and browser Asyncify the core semantics. Those
   can be implementation details only if they remain below the forai task ABI.

## Non-goals

- No public `Future`, `Promise`, or `Task<T>` type in v0 user code.
- No `async` or `await` syntax in v0.
- No cancellation, timeout combinators, streams, channels, or select in v0.
- No preemptive scheduling. Tasks suspend only at known suspension points.
- No parallel CPU execution guarantee. `all` means concurrent progress across
  suspension points, not automatic multi-core execution.
- No broad rewrite of every function. Sync-only functions should keep the
  current direct-WASM lowering where possible.

## Current State

| Primitive | Current behavior |
|---|---|
| Normal user call | Direct WASM `call`, returns an `i64` NaN-boxed value |
| `sleep(ms)` | Host import blocks the current thread |
| `all(...)` | Wraps args in closures, host `run_all` invokes them synchronously |
| `nowait expr` | Wraps expr in a closure, host `spawn` invokes it synchronously |
| HTTP/RPC | Host imports block in CLI/server; browser loader uses sync XHR in places |
| Error propagation | Cross-call `throw` uses globals plus post-call propagation checks |

The existing direct-WASM compiler is sync-stack oriented. Real async requires
resumable function state, not just a different host import.

## Chosen Architecture

forai owns async semantics through a task scheduler and resumable async
function lowering.

Host targets provide only wakeup sources:

- CLI/server: timers, IO completions, future network completions.
- Browser: `setTimeout`, future `fetch`, DOM/event callbacks.

The guest/runtime owns:

- task ids
- task states
- task result slots
- parent/child joins
- `all` aggregation
- error propagation through suspended/resumed calls
- the rule that ordinary calls auto-wait for async callees

## Language Semantics

### Async Effect

Async is an internal effect, not a source type in v0.

A function is async-effectful when:

- it directly calls `wait`;
- it directly calls `all`;
- it directly calls any other async-effectful function;
- it contains a closure that may suspend and that closure is invoked by an
  async primitive;
- later: it calls an async host operation such as HTTP/RPC.

Callers of async-effectful functions automatically become async-effectful
unless the call is inside a task boundary such as `all` or future `nowait`.

### Normal Calls

Source:

```fai
let result = fetchThing(id)
```

Semantics:

- If `fetchThing` is sync, this is the existing direct call.
- If `fetchThing` is async-effectful, the current task starts or enters the
  callee and suspends until the callee completes.
- The expression evaluates to the callee's final return value, never a task
  handle.

### `wait(ms)`

`wait(ms)` suspends the current task and schedules it to resume after at least
`ms` milliseconds.

Open naming decision:

- Existing language docs expose `sleep(ms)`.
- This plan uses `wait(ms)` because that is the requested v0 primitive.
- Implementation can either rename `sleep` to `wait` with a compatibility
  alias, or keep `sleep` and add `wait` as the preferred spelling. Decide
  before implementation and update `language.md`.

### `all(...)`

```fai
let a, b = all(loadA(), loadB())
```

Semantics:

- Each argument expression is evaluated in its own child task.
- Child tasks start before the parent resumes.
- The parent task suspends until all child tasks complete.
- Results are returned as a tuple in argument order.
- If any child throws, the parent observes a throw at the `all(...)` call site.

Initial error rule:

- If multiple children throw, preserve the first completed error and discard
  the rest.
- Future work can expose structured multi-error behavior.

### Closures

Closures can be async-effectful internally. This matters for `all`, future
event handlers, future `nowait`, and array helpers if they ever accept
async-effectful callbacks.

v0 only needs async closures for the synthesized `all` child tasks.

## Runtime Model

### Task State

Each task has:

- `id`
- `status`: `ready`, `running`, `waiting`, `complete`, `failed`
- `entry`: function id plus initial args or closure handle
- `frame`: async frame pointer, when suspended
- `result`: NaN-boxed value or tuple value
- `error`: NaN-boxed error value, if failed
- `waiters`: parent task ids waiting for this task

The scheduler keeps a ready queue and a timer/wakeup table.

### Async Frames

Async-effectful functions lower to resumable state machines. A suspended frame
must preserve:

- resume state label
- locals that are live across suspension
- current expression temporaries that are live across suspension
- callee/child task ids being awaited
- active try/catch context needed for error propagation
- closure environment references or captured values

The compiler should produce frame layouts from liveness analysis rather than
heap-copying every local by default. It is acceptable for v0 to start with a
conservative frame layout if it is correct and bounded.

### Scheduler ABI

Add a small runtime ABI that both browser and CLI/server hosts can drive.

Candidate guest exports:

| Export | Purpose |
|---|---|
| `_start_async() -> i32` | Start the root task, return scheduler status |
| `__fai_poll() -> i32` | Run ready tasks until idle, return status |
| `__fai_resume_task(task_id: i32) -> i32` | Wake a task after host completion |
| `__fai_task_result(task_id: i32) -> i64` | Read final result for tests/runner |

Candidate host imports:

| Import | Purpose |
|---|---|
| `host_set_timer(task_id, ms)` | Schedule a timer wakeup |
| `host_now_ms()` | Existing time source |
| future `host_http_start(...) -> op_id` | Start async IO |
| future `host_cancel_op(op_id)` | Cancel pending host op |

The exact ABI can change during implementation, but the boundary should remain:
guest scheduler owns tasks; host only wakes task ids when external work is
ready.

### Root Execution

Sync programs can continue exporting `_start() -> i64`.

Programs with an async-effectful root need an async runner path:

- CLI/server calls `_start_async`, then polls until root completion.
- Browser loader calls `_start_async`, then keeps polling on wakeups.

Open decision:

- Always emit an async scheduler module, even for sync programs.
- Or emit the existing sync module for sync-only programs and async scheduler
  only when effect analysis requires it.

Recommendation: keep sync-only output on the existing fast path. Add async
output only when needed.

## Compiler Plan

### Phase 1: Effect Analysis

Add a pass after checking and before codegen that computes async-effect
metadata.

Outputs:

- `async_functions: HashSet<FunctionId>`
- `async_closures: HashSet<ClosureSiteId>`
- per-call-site metadata: sync call vs auto-await call
- diagnostics for unsupported async contexts

Rules:

- `wait` marks the containing function async.
- `all` marks the containing function async and treats each argument as a child
  task expression.
- Calling an async function marks the caller async.
- Repeat to fixed point across modules.
- Preserve module/file location information for diagnostics.

Unsupported v0 contexts should fail clearly:

- async callback passed to ordinary array helpers
- async function used where a sync function type is required
- async closure stored for later outside a known scheduler boundary

### Phase 2: Runtime Skeleton

Implement the scheduler data structures and host wakeup stubs without lowering
real async functions yet.

Deliverables:

- task table
- ready queue
- timer registration ABI
- root task lifecycle
- CLI/server runner loop
- browser loader loop
- tests proving a manually wired task can suspend and resume

This phase should not depend on HTTP/RPC or `all`.

### Phase 3: `wait(ms)`

Lower `wait(ms)` as the first real suspension point.

Compiler work:

- async-lower a minimal function containing `wait`
- frame stores resume state and live locals
- `wait` registers timer and returns control to scheduler
- resume continues after `wait`

Runtime/host work:

- CLI/server: use a timer mechanism that does not block the whole process.
- Browser: use `setTimeout` to call back into `__fai_resume_task`.

Acceptance:

```fai
def main
    @return Int
do
  wait(10)
  42
end
```

returns `42` in CLI/server and browser.

### Phase 4: Auto-Await Calls

Support calling an async-effectful function from another function.

Compiler work:

- sync callers of async callees become async through effect analysis
- call lowering starts/enters callee task/frame
- parent suspends until callee completes
- result is restored as the expression value
- thrown errors propagate at the call site

Acceptance:

```fai
def child
    @return Int
do
  wait(10)
  7
end

def main
    @return Int
do
  let x = child()
  x + 1
end
```

returns `8` without `await` syntax.

### Phase 5: `all(...)`

Replace the current synchronous host `run_all` model with scheduler-owned child
tasks.

Compiler work:

- keep the existing useful shape of wrapping each argument expression in a
  zero-arg closure/task expression
- spawn all child tasks through the guest scheduler
- suspend parent until all children finish
- build the result tuple in argument order
- route first child error through the parent call site

Runtime work:

- child task tracking
- join counters
- tuple result allocation
- failure propagation

Acceptance:

```fai
def slow
    @return Int
do
  wait(50)
  1
end

def fast
    @return Int
do
  wait(10)
  2
end

def main
    @return Int
do
  let a, b = all(slow(), fast())
  a + b
end
```

returns `3`, and wall-clock behavior proves the tasks overlap.

### Phase 6: Errors, Try/Catch, and Finally

The existing throw path uses globals and post-call propagation. Async must
preserve the same source semantics across suspension.

Tasks:

- represent failed task state separately from complete task result
- resume waiting parent with error information
- make `try/catch` catch errors thrown after suspension
- decide `finally` behavior for suspended frames and child task failures
- add tests for direct call, nested call, and `all`

Acceptance:

```fai
def child
    @return Int
do
  wait(10)
  throw Error('boom')
end

def main
    @return String
do
  try
    let x = child()
    'bad'
  catch err
    err.message
  end
end
```

returns `'boom'`.

### Phase 7: Browser/CLI Parity Harness

Add feature fixtures that run in both targets.

Fixtures:

- wait returns after resume
- locals survive across wait
- nested auto-await
- `all` result ordering
- `all` overlaps timers
- child error in `all`
- try/catch after wait
- module function async propagation
- closure capture across wait

The same `.fai` sources should be used where possible for CLI/server and
browser assertions.

### Phase 8: Replace Old Concurrency Imports — DONE

Status (2026-06-03):

- **Naming decided (Open Question 1):** `wait` is removed entirely; `sleep(ms)`
  is the sole timed-suspend primitive. Calls auto-await by default, so a `wait`
  spelling read like an await keyword. `wait` removed from the checker builtin,
  codegen dispatch, async-effect analysis, the scheduler's `wait_call_delay_ms`,
  `language.md`, `concurrency.fai`, and all fixtures. `sleep`/`all`/`nowait` are
  the only concurrency surface.
- **Synchronous imports scoped to the test/legacy path.** Production async
  (`fai run`, browser; `is_test=false`) routes through `try_codegen_async` →
  scheduler / `host_set_timer` and never emits or calls `sleep_ms`/`run_all`.
  Those imports are reachable only when async analysis declines — i.e.
  `is_test=true`, where async functions called from `test` blocks fall through
  to the direct builder. There the host `sleep_ms` (blocking) and `run_all`
  (sequential tuple) give correct *values* for synchronous test assertions
  (tests assert values, not timing/overlap). They are documented as the
  test-mode / legacy-direct path in `host/async_ops.rs` and `direct.rs`.
- Browser loader shims for `sleep_ms`/`run_all` throw (browser is always
  `is_test=false` and strips those imports); messages updated to drop `wait()`.
- `language.md` + `fai-cli/docs/lang/concurrency.fai` updated.
- Full workspace green; all concurrency + browser_async fixtures pass.

Remaining (future, not blocking): retiring `sleep_ms`/`run_all` *entirely* would
require routing the test runner (`_fai_run_test`) through the scheduler so async
test bodies suspend/resume like production. Deferred — see "uniform async in
tests" under Future Extensions.

## Future Extensions

The design should intentionally leave room for:

- `nowait expr` as fire-and-forget task spawn
- HTTP/RPC as async host operations
- browser event handlers that resume tasks
- async event delivery
- async array helpers or explicit task combinators
- cancellation and timeouts
- structured task handles, if user-facing tasks become necessary
- server request concurrency

Each extension should add a scheduler primitive or host wakeup source, not a
new target-specific async semantics layer.

## Diagnostics

Async diagnostics need to be specific because effect propagation can otherwise
feel mysterious.

Required examples:

- `async callback cannot be passed to sync function type`
- `async operation is not allowed in this top-level initializer`
- `all child expression cannot capture mutable binding by reference`
- `async lowering unsupported for <construct> at file:line:col`

Every diagnostic must include file and line. Codegen refusals must not report
bare internal helper names as user identifiers.

## Testing Strategy

Unit tests:

- effect analysis fixed-point propagation
- frame layout/liveness for locals across suspension
- scheduler task state transitions
- timer wake/resume
- all join behavior
- error propagation

Integration fixtures:

- language feature fixtures under `fai/tests/fixtures/language/concurrency/`
- browser harness fixtures for parity
- CLI run/test fixtures for native Wasmtime runner

Performance checks:

- sync-only programs should still use the existing direct fast path
- async frame allocation should be proportional to live state, not total source
  size
- `all` should not serialize child execution

## Risks

1. **Frame lowering complexity.**

   Resumable frames are the hard part. Start with a minimal subset for `wait`
   and grow coverage only with tests.

2. **Error propagation across suspension.**

   The current global error flag approach may not be enough once multiple tasks
   exist. Task-local error state is likely required.

3. **Mutable captures and value semantics.**

   forai has deep-copy value semantics and explicit mutable params. Async child
   tasks must not introduce shared mutable aliasing accidentally.

4. **Browser runner reentrancy.**

   JS wakeups may happen while polling is already active. The browser loader
   needs a simple guard/queue around `__fai_poll`.

5. **Sync/async function type boundary.**

   v0 should reject async callbacks in sync function slots. Adding async
   function types later is a separate design.

## Open Questions

1. ~~Should the user-facing primitive be `wait(ms)`, `sleep(ms)`, or both?~~
   **Resolved (2026-06-03): `sleep(ms)` only.** `wait` removed — calls
   auto-await by default, and `nowait` is the only concurrency keyword.
2. Should async-effectful named functions be visible in docs as async, even
   without source syntax?
3. Should `all()` with zero arguments be allowed? If yes, what tuple shape?
4. Should `all` cancel remaining children after the first error, or let them
   finish? v0 recommendation: let them finish unless cancellation exists.
5. Do async root programs keep exporting `_start`, or switch to `_start_async`
   only? Recommendation: keep `_start` for sync-only modules.
6. How much liveness precision is required in v0 frame layout?

## Implementation Order Summary

1. Add async-effect analysis and diagnostics.
2. Add scheduler runtime skeleton and host wakeup ABI.
3. Implement `wait` with resumable async frames.
4. Implement auto-await calls.
5. Implement scheduler-owned `all`.
6. Make throw/try/catch correct across suspension.
7. Add browser/CLI parity fixtures.
8. Retire old synchronous concurrency imports or map them into the scheduler.

