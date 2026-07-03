# Known Issues

Tracked gaps and bugs in the forai language, stdlib, and tooling. Newest at
the top. When fixing one, move it to a "Resolved" note or delete it with the
fixing commit.

## Open

### async engine: sync throw after catching a failed async task hangs the scheduler

- **Date:** 2026-07-03
- **Area:** async engine (error state / park) — pre-existing, found while
  fixing the bare-string throw bug (reproduces on the unmodified baseline).
- **Severity:** high — `fai run` parks forever, no output, no timeout.

Shape: `main` catches a **failed async task** (a function that awaits and
then throws to its awaiter), then calls **any sync function that throws**
inside a second `try`/`catch`. The second throw never reaches the catch;
the scheduler parks forever. The same second call *not* throwing (or no
prior async failure) runs fine, and the test-runner lane runs both
functions fine — only the main-path sequence hangs. Likely stale
error/park state left by the async-fail catch path.

```
def boom      # sleep(1) then throw Error('x')
def rethrow   # throw Error('original')   (sync)

def main do
    try boom() catch e print(e.message) end       # prints, ok
    try rethrow() catch e print(e.message) end    # ← hangs here
end
```

Repro fixture (skipped):
`tests/fixtures/language/errors/async_fail_then_sync_throw/`.

### async engine: an error thrown across `fail`/catch leaks the error object

- **Date:** 2026-07-03 (pre-existing; measured while fixing bare-string throw)
- **Area:** async engine catch-var ownership (plan-117 async-runtime-root family)

A `throw Error('x')` that crosses the async `fail` → awaiter-catch path
leaks the error dict + its two strings (3 objects per throw) — measured
identical on the unmodified baseline with `--check-leaks` over a variant
of `tests/fixtures/language/reclaim/async_throws_reclaims/`. The awaiting
catch binds the error without a matching release (same borrowed-catch-var
convention noted at `direct.rs` `Term::ThrowTo`). Bare-value throws now
box into an Error-shaped wrapper (2026-07-03), so they exhibit the same
per-throw leak instead of none. Safe but unbounded for servers that throw
across tasks in a loop; belongs with the `# leak: expected`
async-runtime-root ratchets when the catch-var convention is fixed.

### test-mode codegen: UFCS call in an entry file fails the test step with a misattributed UnknownIdentifier

- **Date:** 2026-06-11
- **Area:** UFCS metadata / test-mode compilation
- **Found by:** Plan 118 phase-2 work — a probe with `'a,b,c'.split(',')` in `main`.

A UFCS method call in an ENTRY file (e.g. `'a,b,c'.split(',')` inside
`def main`) passes `fai check` but fails the test-step compile of the
`fai run`/`fai test` pipeline with `UnknownIdentifier("split")` at the
call site — the run-step compile of the same file succeeds. Fixture
programs under `tests/fixtures/language` use UFCS and pass, so the
break is specific to how the entry file is prepared for the test step.

Suspected cause: the checker's `ufcs_calls` set is keyed by
`(module_key, line, col)`; test-mode source handling (synthetic test
wrapping / module attribution for the entry file) shifts or re-keys
those positions, so codegen misses the UFCS rewrite and falls through
to bare-identifier resolution.

Repro fixture (skipped): `tests/fixtures/language/ufcs/entry_file_test_step/`.
Until fixed, plan-118 fixtures avoid UFCS-in-entry shapes where the
test step matters.

### runtime: caught `e.message` can read stale garbage when thrown before DB init

- **Date:** 2026-06-01
- **Area:** runtime (exception message handling)
- **Found by:** Brain, after a refactor reordered tests.

A test caught an `Error('path not found: …')` thrown by a function that does no
DB access, but `e.message` read as `"forsqlite.exec failed: no such table:
skills"` — a stale forsqlite error-buffer value — and only in full-suite
ordering where the shared DB (`getDb()`) had not yet been initialized when the
throw occurred. Forcing `getDb()` before the throw makes `e.message` read
correctly. So a caught exception's `.message` can surface unrelated/leaked bytes
depending on prior runtime state (here, whether the sqlite layer was
initialized). Was suspected to share a root cause with the bare-`throw`
bug (now fixed — see Resolved, 2026-07-03: non-dict throws box into an
Error-shaped dict); worth re-running brain's order-sensitive repro against
the fixed runtime before investigating further. The tell is `e.message`
containing text the throwing code never produced.

### stdlib: no directory-creation primitive in `std.file`

- **Date:** 2026-05-31
- **Area:** stdlib / `std.file`
- **Found by:** Brain project (`forai/brain`), filesystem skill tests.

`std.file` exposes `read`, `write`, `exists`, and `list`, but there is no way
to create a directory. The builtins are declared in
`crates/fai-checker/src/builtins/file.rs` (`fileRead`, `fileWrite`,
`fileExists`, `fileList`) with no `mkdir` / `createDir` / `makeDirs`
counterpart.

Consequences:

- forai code cannot create a directory using the stdlib alone. The only
  workaround is shelling out via `std.process` (`process.run("mkdir -p …")`),
  which is heavier, platform-dependent, and unavailable on hosts where process
  spawning is restricted.
- Any feature that needs to lay down a nested output path (reports, artifacts,
  build output, scaffolding) has no first-class way to ensure the parent
  directory exists.

Proposed fix: add `file.makeDir(path)` and/or `file.makeDirs(path)` (recursive)
builtins, backed by `std::fs::create_dir` / `std::fs::create_dir_all` in the
host runner. Consider also `file.remove` / `file.removeDir` for symmetry, since
tests and skills currently shell out for cleanup too.

### stdlib: `file.write` does not create parent directories

- **Date:** 2026-05-31
- **Area:** stdlib / `std.file`
- **Found by:** Brain project (`forai/brain`), filesystem skill tests.

`file.write(path, text)` maps directly to `std::fs::write` in the host runner
(`crates/fai-cli/src/wasm_runner/host/io.rs`, ~line 114). `std::fs::write` does
**not** create missing parent directories, so writing to a path like
`/tmp/new_dir/file.txt` when `/tmp/new_dir` does not exist fails.

~~The failure is also silent at the call site~~ **Update 2026-07-02:** the
silent-failure half is fixed — `file.read`, `file.write`, and `file.list`
failures now raise a guest-catchable error carrying the path and OS reason
(error-channel migration; pinned by
`tests/fixtures/language/stdlib/file_errors_catchable/` and
`file_read_missing_uncaught.invalid/`). What remains open here is the
missing-primitive half: without `file.makeDirs` there is still no
first-class way to write into a new nested path — the write now fails
*loudly*, but it still fails.

Proposed fix (remaining): decide between (a) `file.write` creating parent
directories automatically (`create_dir_all` before `fs::write`), or (b) keep
it strict and pair it with a `file.makeDirs` primitive (see the issue above).

## Resolved

### rc: double-free in brain's data.hooks.memorySearchOutput under FAI_RC_CHECK

- **Date:** 2026-06-11; no longer reproduces as of 2026-07-03.

`FAI_RC_CHECK=1 fai test` over brain's full inline suite is now clean —
`data.hooks.memorySearchOutput` passes (3 cases) and both target lanes
report 2095 + 1968 passed with no rc-check traps. The specific fixing
change is unconfirmed: candidates are the 2026-07-03 non-dict throw
boxing (hooks paths throw), the file-error error-channel migration, or
the plan-120/121/122 ownership-ABI phases landed since the report. If a
regression resurfaces, start from the hooks subtree's throw/error paths.

### language: bare `throw <string>` produces a corrupt, memory-unsafe `e.message`

- **Date:** 2026-05-31, resolved 2026-07-03 (semantics option (a)).

Non-dict thrown values are now boxed at the throw site into an
Error-shaped `{message: toString(value)}` dict — in all three lowering
paths (sync `compile_throw`, async `Term::ThrowTo`, async `Term::Fail`)
— so a caught `e.message` is always a valid, memory-safe String and
`string.trim(e.message)` no longer traps. Dicts (Error values, thrown
records) pass through untouched. Pinned by
`tests/fixtures/language/errors/throw_bare_string/` (unskipped, with
`leak: flat` + `ownership: balanced` gates) and
`errors/throw_bare_string_async/`; documented in language.md.
Note: bare throws now allocate the wrapper, so they participate in the
pre-existing async fail/catch error-object leak (tracked above).

### codegen: tail-position `from_dict` passes check but breaks build, with a misattributed error

- **Date:** 2026-06-01, resolved 2026-07-03.

Fixed in two parts. (1) Tail/return-position `from_dict(d)` in a function
whose `@return` is a plain named type is now desugared into the
typed-binding form (`fai-compiler::desugar`, applied in
`convert_program` so the checker and both codegen paths agree) — the
ISSUES.md repro compiles and runs. (2) Remaining unsupported positions
(argument position; optional/array/generic target types) get a dedicated
`from_dict-without-typed-binding` diagnostic with the `let x T =
from_dict(d)` suggestion, instead of `UnknownIdentifier("from_dict")`
whose best-effort location walk pointed at an unrelated module's
legitimate `from_dict`. Pinned by
`tests/fixtures/language/types/from_dict_tail_position/` (unskipped),
`types/from_dict_arg_position.invalid/`, and fai-compiler desugar unit
tests.

### build: two `rpc_server` targets in one project share a single RPC surface

- **Date:** 2026-06-01, resolved by plan 100 (reachable RPC surface),
  regression-pinned 2026-07-02.

RPC surface discovery is now scoped to each target's reachable graph, so two
`rpc_server` targets over one `src` tree build independently. Pinned by
`crates/fai-feature-tests/tests/two_rpc_targets.rs` over the tracked project
fixture `tests/fixtures/projects/two_rpc_servers/`.
