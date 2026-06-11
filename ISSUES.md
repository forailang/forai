# Known Issues

Tracked gaps and bugs in the forai language, stdlib, and tooling. Newest at
the top. When fixing one, move it to a "Resolved" note or delete it with the
fixing commit.

## Open

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

### codegen: tail-position `from_dict` passes check but breaks build, with a misattributed error

- **Date:** 2026-06-01
- **Area:** checker/codegen discrepancy (`from_dict` builtin)
- **Found by:** Brain — a webhook test/util that built an `HttpRequest`.

`from_dict` needs an explicit target-type annotation to codegen. The working
form is:

```
let x T = from_dict(dict)
x
```

Using it in **tail/return position** — relying on the function's `-> T` return
type to supply the target type — type-checks fine (`fai check` is green) but
**fails codegen** (`fai build` / `fai test`). Two things make it costly to
diagnose:

1. The error is **misattributed**: it surfaces at the *next* package that uses
   `from_dict`, not the offending call site. Ours pointed at `Forui.rpc`
   `withCookies` (`UnknownIdentifier("from_dict") (line 162:35)`) even though
   that code was correct and unchanged — the real culprit was a tail-position
   `from_dict` in the Brain app.
2. `fai check` and the codegen disagree, so the problem only appears after a
   green check, in the build/test step, in a dependency.

Minimal repro:

```
def make -> HttpRequest do
    var d = {}
    d = set(d, 'path', '/x')
    from_dict(d)   # tail position — checks OK, breaks build
end
```

Workaround: always bind via `let x T = from_dict(...)`. Proposed fix: make
codegen honor the return-type annotation for tail-position `from_dict` (match
the checker), or have the checker reject tail-position `from_dict` with a
clear, correctly-located error. Related to forai #1/#5 (built-in types like
`HttpResponse`/`HttpRequest` not constructible by name, forcing the
`from_dict` round-trip in the first place).

### build: two `rpc_server` targets in one project share a single RPC surface

- **Date:** 2026-06-01
- **Area:** CLI / multi-target build (RPC dispatch + surface discovery)
- **Found by:** Brain — trying to add a standalone task server as a second
  `rpc_server` target alongside the existing web/server targets.

A project's RPC surface is discovered from the project `source_root`, not the
per-target `source`. Consequences when adding a second `rpc_server` target:

- The new target's generated `__rpcDispatch` includes **every** `remote def`
  in the shared `src` tree (so a task server inherited Brain's `tools_call`,
  `capabilities_search`, etc.), and fails to compile when one of those remote
  defs returns a type not reachable from the new target's `main`
  (`Unknown type 'SkillExecutionResult'` from generated dispatch code).
- Setting the target's `source` to a different folder does **not** redirect RPC
  discovery — it still reads `<source_root>/<module>` and errors
  (`cannot read module directory 'src/taskserver'`).

Net: you can't host two *independently-scoped* RPC services as targets of one
`fai.toml`. The practical resolution is a **separate fai project** (own
`fai.toml` + `source_root`), with the services talking over HTTP. That's what
Brain's task server became (`brain/task_server/`).

Proposed fix: scope RPC surface discovery + dispatch generation to the target's
own `source`/reachable graph, so multiple isolated `rpc_server` targets can
coexist in one project. Until then, document that isolated services need
separate projects.

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
initialized). Likely the same underlying string/exception memory issue as the
bare-`throw` bug below. Repro is order-sensitive; the tell is `e.message`
containing text the throwing code never produced.

### language: bare `throw <string>` produces a corrupt, memory-unsafe `e.message`

- **Date:** 2026-05-31
- **Area:** language / runtime (exceptions + string representation)
- **Severity:** high — memory-unsafe (wasm OOB access), silent data corruption.
- **Found by:** Brain project (`forai/brain`), routed tool-call audit work.

Throwing a bare string and catching it yields a `message` that is not a
well-formed String. Throwing via the `Error(...)` constructor works correctly.

Minimal repro (no DB, no deps):

```forai
def boomString
    @return Void
do
    throw 'hello from string'
end

def boomError
    @return Void
do
    throw Error('hello from Error')
end

def main
    @return Void
do
    try
        boomError()
    catch e
        print(e.message)            # -> "hello from Error"  (correct)
        print(string.trim(e.message))   # ok
    end
    try
        boomString()
    catch e
        print(e.message)            # -> "null"  (WRONG; should be the thrown string)
        print(string.trim(e.message))   # -> wasm trap: out of bounds memory access
    end
end
```

Observed with bare-string throws:

- `e.message` renders as `null` (so `'' + e.message` is the 4-char string
  `"null"`), i.e. the thrown text is lost.
- Operations that read the value's raw pointer/length — `string.trim(e.message)`,
  or binding it as a SQL parameter via `Forsqlite.exec_params` — fault or
  corrupt state. The wasm trap address is ASCII character data (e.g.
  `0x6f77207b` = bytes of the thrown text), i.e. string bytes are being
  dereferenced as a pointer.
- Operations that copy char-by-char (`print`, `'prefix' + e.message`) happen to
  succeed, which masks the problem until a raw-pointer consumer touches it.

This surfaced in Brain as `forsqlite.exec_params finalize failed` when an audit
row tried to persist a caught validation error's `e.message`: the bare-string
throw made the bound parameter malformed and SQLite's `step`/`finalize` failed.
Switching the throw sites to `throw Error(...)` (the idiom the rest of the
codebase already uses) fixed it.

Proposed fix: decide on `throw` semantics for non-`Error` operands. Either
(a) box a bare-string throw into a proper error whose `.message` is that string,
or (b) reject `throw <non-Error>` at type-check time. Either way, a caught
`e.message` must always be a valid, memory-safe String. The current silent
corruption + OOB access is the dangerous part.

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

The failure is also silent at the call site in a misleading way: `fileWrite`
returns `0` (→ `false`) on error rather than throwing, so callers that ignore
the boolean result get no signal that the write was dropped. Combined with the
missing `mkdir` above, there is no clean way to write into a new nested path.

Proposed fix: decide on intended semantics and make them consistent —
either (a) have `file.write` create parent directories automatically
(`create_dir_all` on the parent before `fs::write`), or (b) keep it strict but
document that the parent must exist and pair it with a `file.makeDirs`
primitive (see the issue above). Either way, consider surfacing write failures
more loudly than a bare `false`.
