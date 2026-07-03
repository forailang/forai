# Language feature fixtures

Each `.fai` file here is a self-contained language feature test. A thin
Rust harness (`crates/fai-feature-tests`) walks this tree and runs each
fixture through a set of gates: format check, type check, wasm compile,
and native execution.

Adding a fixture is how you add language-feature coverage. Writing a
new `#[test]` in Rust should be rare.

## Directory layout

Each fixture is a **directory** containing a `main.fai` file. Fixtures
are grouped by language feature:

```
fixtures/language/
  smoke/hello/main.fai
  variables/let_inferred/main.fai
  variables/let_reassign.invalid/main.fai
  variables/let_from_int_fn/main.fai
  operators/and/main.fai
  operators/ampamp.invalid/main.fai
  ...
```

Feature groups (parent dirs) contain no `.fai` files directly — only
fixture subdirectories. The harness walks the tree, spots every dir
that contains a `main.fai`, and runs it as a fixture.

**Why subdir-per-fixture?** The fai pipeline's test step scans all
sibling `.fai` files in a fixture's parent directory to aggregate
public functions and test blocks (it assumes a real project where
siblings are imported into one compilation unit). Flat fixtures would
cross-pollinate: `test returnsInt` in one fixture would affect
coverage accounting in its unrelated flat neighbour. Isolating each
fixture in its own directory gives the pipeline exactly what it
expects — a self-contained mini-project.

Feature groups planned:

```
smoke/              — minimal end-to-end sanity
variables/          — let, var, annotations, shadowing
literals/           — Int, Float, Bool, String, null
operators/          — arithmetic, comparison, boolean, strings
control_flow/       — if, match, while, for, break, continue, return, try
loops/              — iteration patterns
functions/          — named, params, defaults, recursion
closures/           — do..end, trailing closures, captures
modules/            — imports, visibility
types/              — type def, fields, generics
arrays/  dicts/  ufcs/  stdlib/  ffi/  testing_syntax/  browser/
```

Project/package repros that need `fai.toml` and local dependencies live in a
parallel tree:

```
fixtures/projects/
  local_dep_ownership/
    fai.toml
    src/main.fai
    dep/fai.toml
    dep/src/helper.fai
```

For project fixtures, directives still live in the leading comments of
`src/main.fai`, but the harness runs CLI commands from the project root instead
of passing a single file path. Use this lane when an app bug only reproduces
through project/package resolution, `file://` dependencies, or source-root
behavior. Keep the repro minimal and self-contained inside `fai/` unless the
bug specifically requires an external workspace project.

Fixture names are lowercase snake_case describing the concept. Invalid-
by-design fixtures end in `.invalid`:

```
variables/let_reassign.invalid/main.fai
operators/ampamp.invalid/main.fai
```

Display names in `cargo test` output use `::` separators:

```
test variables::let_inferred ... ok
test operators::ampamp.invalid ... ok
test projects::local_dep_ownership ... ok
```

## Directive block

Every fixture begins with a block of `#` comments that tells the harness
what to expect. The directive block is terminated by the first non-`#`
line (including blank lines).

```fai
# expect: ok
# stdout:
#   42
#   hello

def main
    @return Void
do
  print(42)
  print('hello')
end
```

### Supported directives

| Directive | Meaning |
|---|---|
| `expect: ok` | fmt-check, check, compile, and run all succeed. Default when omitted. |
| `expect: check_error` | Checker must reject. Requires `error:`. |
| `expect: compile_error` | Wasm compiler must reject after checker passed. Requires `error:`. Rare. |
| `expect: runtime_error` | Program executes but traps or throws. Requires `error:`. |
| `stdout:` | Expected stdout. Each following `#` line (indented under the directive) is one output line. Trailing newline tolerated. |
| `error:` | Pattern the diagnostic must contain. Plain substring, or `/regex/` for a regex. |
| `error_at: <line>:<col>` | The matched error must be attributed to this source position: one output line must contain both the `error:` pattern and `(line <line>:<col>)`. Requires `error:`. Use it to pin diagnostic *attribution* (plan 130 A1) — a fixture passes only if the error points at the right statement, not just anywhere in the file. |
| `skip: <reason>` | Temporarily skip. Must reference a tracking issue. |
| `browser:` | Browser assertion. Use `selector:` plus `text:`/`html:` for DOM assertions, `rootResult:` for the root return value, optional duration bounds for async parity, and optional `click:` actions for browser event coverage. |
| `leak: flat` | Leak gate (plan 118): re-run under `--check-leaks`; the program must end with zero live heap objects. Locals release at scope exit, so a flat fixture avoids module-level `var`s (they stay live by design). |
| `leak: expected <phase-tag>` | The fixture leaks today; `<phase-tag>` names the plan-117 phase that fixes it. Two-sided: when the leak is fixed, the gate FAILS with "flip the marker" — the fixing change must flip this to `leak: flat`. Fixtures without a `leak:` directive are ungated. |
| `ownership: balanced` | Native ownership gate (plan 123): re-run under `--check-ownership`; the helper ownership report must say zero object imbalances. This is independent from `leak:`. |

A fixture carrying both `browser:` and `leak:` runs the leak gate inside the
browser instead of natively: after the root completes, the harness reads the
always-exported `__live_objects` counter via `window.__fai_live_objects()`
with the same two-sided semantics. Categories that cannot run in the browser
simply never carry `browser:`.

A browser fixture can also include `#   ownership: balanced` inside its
`browser:` block. The harness builds with ownership instrumentation and calls
`window.__fai_assert_ownership()` after root completion. Native fixtures use the
top-level `ownership: balanced` directive instead.

Browser fixtures can include `#   click: <selector> <count>` to exercise DOM
event bridges before the browser leak or ownership gate runs. Use this for
reduced browser-only leaks, especially when host-created event payloads cross
into wasm.

### Leak baseline suite (`rc/`, `rc_browser/`)

The plan-118 baseline: every plan-117 feature category pinned with a
`leak:` directive. This table is the authoritative category map. Guest-pure
categories compile identically for both wasm targets and are validated
natively; the browser-specific leak surface (JS host imports) is covered by
`rc_browser/`. The brain request loop (category 14) lives in the brain
project's test lane, not here.

| # | Category | Fixtures | State |
|---|----------|----------|-------|
| 1 | binding + discard | `rc/binding_discard`, `rc/binding_scope_exit`, `rc/print_int_scratch`, `rc/template_interpolation_loop` | flat; `binding_discard` also ownership-balanced; interpolation temporaries release after concat |
| 2 | assignment/overwrite | `rc/assign_overwrite` | flat |
| 3 | destructuring | `rc/destructure_tuple` | flat |
| 4 | arrays + helpers | `rc/array_helpers`, `rc/array_map_bind`, `rc/array_filter_discard`, `rc/receiver_alias_sort`, `rc/receiver_alias_array_rebuilders` | flat, including audited fresh receiver array rebuilders |
| 5 | dict/field stores | `rc/dict_field_store` | flat |
| 6 | std host imports | `rc/std_json_roundtrip`, `rc/std_path_join`, `rc/json_parse_array`, `rc/json_require_string` | flat — incl. the Owned string/graph classes and the verified-Borrowed alias class (plan 119; requireString returns the dict entry's own pointer) |
| 7 | FFI externs | `rc/ffi_libc_abs` | flat (primitive returns) |
| 8 | events | `rc/events_off`, `rc/events_clear`, `rc/events_once` | flat — host-retained handlers release on off/clear/once removal |
| 9 | async frames | `rc/async_frame_complete` | frames released; scheduler buffer → expected async-runtime-root |
| 10 | break/continue | `rc/break_scope`, `rc/continue_scope`, `rc/loop_fallthrough` | flat |
| 11 | throw/catch | `rc/throw_caught`, `rc/try_in_loop`, `rc/loop_in_try`, `rc/throw_through_two_frames` | flat, including in-function catch and cross-function propagation cleanup |
| 12 | closures | `rc/closure_capture` | balances; typedef callbacks pull async scheduler → expected async-runtime-root |
| 13 | spy/mock | `rc/spy_mock_reset` | run-path flat; host retention is the `fai test --check-leaks` lane's oracle (phase 6 audit) |
| 14 | router handlers | `rc/router_reset` | flat — retained route handlers release on finite run/test teardown |
| 15 | brain request loop | brain project inline suite | see plans/118 U8 |

Browser-host leak surface: `rc_browser/flat_baseline` (flat),
`rc_browser/sethtml_literal_arg` (literal arg temp flat),
`rc_browser/events_registry` (event registry flat).

### Ownership reduction workflow

Use `fai run <fixture-or-repro> --check-ownership` or
`fai test <fixture-or-repro> --check-ownership` when a memory bug smells like a
missing retain/transfer, duplicate cleanup, skipped return/discard cleanup, or
host-call argument convention bug. The report names grouped operation clusters
first, then source line when available, aux detail, and recent per-object
history. Reduce the app case into the matching `rc/`, `rc_browser/`, or
`fixtures/projects/` category, then add `ownership: balanced` once the reduced
fixture is clean.

Instrumentation itself has a seeded validation lane:
`FAI_OWNERSHIP_SEED=suppress-retain fai test <fixture> --check-ownership`
suppresses one helper event family and should fail with a resolved site history.
The meaningful seeded failure families today are `suppress-retain`,
`suppress-transfer`, and `suppress-cleanup`; unknown seed names warn and remain
inactive. Keep seeded cases in Rust integration tests or explicit `.invalid`
fixtures; do not mark seeded fixtures as normal `ownership: balanced`.

## Gates

For `expect: ok` fixtures:

1. **fmt-check** — `fai fmt <path> --check` must accept the file as
   canonical. Fixtures are kept in canonical form so the harness never
   mutates them.
2. **check** — `fai check <path> --check` must pass.
3. **compile + run** — `fai run <path> --check` must exit 0. The
   pipeline implicitly exercises wasm compilation.
4. **stdout diff** — captured stdout (ignoring trailing whitespace) must
   match the `stdout:` directive.

For `expect: check_error` fixtures: gate 1 is skipped (the program may
not parse); gate 2 must fail with `error:` matching stderr.

For `expect: compile_error` fixtures: gates 1 and 2 pass; gate 3 fails
at compile time with `error:` matching stderr.

For `expect: runtime_error` fixtures: gates 1–3 reach execution, which
must fail with `error:` matching stderr (or trap output).

For browser fixtures, use `expect: ok` with a `browser:` block:

```fai
# expect: ok
# browser:
#   selector: #app .message
#   text: Hello
```

The harness runs fmt-check and check, builds the fixture with
`fai build --html`, serves the generated bundle with the contained
Playwright runner in `tests/browser-harness`, and asserts either the
selected element's `innerText` (`text:`) or `innerHTML` (`html:`).

Async/browser parity fixtures can assert a returned root value directly:

```fai
# expect: ok
# browser:
#   rootResult: 42
```

Async browser fixtures can also assert root execution duration after the
WASM instance starts:

```fai
# expect: ok
# browser:
#   rootResult: 3
#   durationAtLeastMs: 80
#   durationLessThanMs: 170
```

Browser event fixtures can click a target before asserting leaks or ownership:

```fai
# expect: ok
# browser:
#   selector: #click-target
#   text: Click me
#   click: #click-target 25
# leak: flat
```

### Scheduler and blocking host I/O fixtures

`nowait` is cooperative. Fixtures that assert interleaving should use a real
suspension point such as `sleep`, `all`, an auto-awaited async call,
`remoteCall`, or a stdlib operation that lowers through the generic host-op
await path. Blocking host I/O fixtures should prove ordering with an observable
peer task or delayed local server rather than only checking that a value
returns.

Known blocking host-I/O surfaces such as `std.http.request.*`,
`std.process.run`, `std.file.read/write/list`, `std.env.load`, raw TCP waits,
and `std.net.udp.receive` are expected to park the current task and resume with
owned results materialized on the scheduler thread. CPU-bound direct helpers
such as `std.array` closure helpers, `std.json`, and `std.crypto` are a separate
fairness problem; do not use them as evidence that `nowait` has independent
thread/process isolation.

## Conventions

- **Canonical only.** Run `cargo run --bin fai -- fmt <fixture>` once
  when adding a fixture to normalise it, then leave it alone.
- **Small.** One concept per fixture. If you need two features to
  demonstrate one, you are adding two fixtures.
- **No skips without a link.** A `skip:` directive with no tracking
  issue fails the harness.
- **Print what you want to check.** Semantics are asserted via `stdout`.
  If the feature has no observable output (e.g. a pure compile-time
  rule), put it in an `.invalid.fai` fixture instead.
