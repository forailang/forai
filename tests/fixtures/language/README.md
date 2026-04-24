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
| `skip: <reason>` | Temporarily skip. Must reference a tracking issue. |
| `browser:` | Browser DOM assertion. Requires `selector:` plus `text:` or `html:` continuation lines. |

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
