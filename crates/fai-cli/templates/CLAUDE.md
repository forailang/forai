# __FAI_PROJECT_NAME__

## Development Process

forai treats testing and documentation as first-class — they're enforced by
the tooling, not optional style. Read this before writing code.

- **Document as you go.** Every named `def`, `remote def`, and `test`
  block requires a `# Description.` line directly above it. Missing
  one is a type error: `doc comment required`. `main` is exempt.
- **Test every function.** `fai test` fails the build if any public
  function is uncovered. Private helpers covered by a tested caller are OK.
- **Red → green → refactor, one function at a time.** Write the `test`
  block first with 1–3 `it` cases, run `fai check` to catch signature
  errors, then fill in the body until `fai test` passes. Don't write
  five functions and then build — each failure hides the next.
- **Use `fai_examples` before writing a new kind of code.** Keywords:
  `rpc`, `http`, `ui`, `children`, `types`, `testing`, `function`, `fai.toml`.
  Faster than rediscovering the pattern from error messages.

## Language Quick Reference

**fai.toml** — project config. `fai_examples` for complete config patterns. `fai doc lang` for full reference.

**Functions** — every public function needs a doc comment and a test:
```fai
# Add two numbers.
def add
    @param a Int
    @param b Int
    @return Int
do
  a + b
end

test add
it 'adds numbers'
  assert.equals(add(1, 2), 3)
end
end
```

**Modules** — a directory is a module, import by directory name not filename:
```fai
use { HomePage } from client.pages          # src/client/pages/*.fai
use { Label, Button, fontSize } from Forui.view  # must import each UFCS function
```

**Testing** — required for every public function or `fai test` fails.
**Types** — `type Task { id Int\n  text String\nend }` · constructed with named args · `Task(id: 1, text: 'x')`.
Built-ins: `Int`, `Float`, `String`, `Bool`, `Void`, `T[]`, `T?`. Arrays need commas: `[1, 2, 3]`.
Dicts: `getString(d, 'k') -> String?`, `getInt(d, 'k') -> Int?`, `set(d, 'k', v) -> Dictionary`.
Optionals: `x?` checks non-null · `x!` unwraps · `unwrap(x, fallback)` safe unwrap.

## CLI Commands

```bash
fai fmt            # format source files in src/
fai check          # fmt → type-check
fai test           # fmt → check → run tests (REQUIRED for all functions)
fai run            # fmt → check → test → run
fai build          # fmt → check → test → build
fai doc <query>    # look up docs: 'lang', 'std.array', 'fontSize', 'Forui'
fai_examples       # MCP tool: complete working code patterns
```

Output shows one `[ok]` / `[fail]` line per pipeline step (fmt, check,
test, build). Pass `-v` for per-file details. An uncovered public
function counts as a failed test — same exit code as an assertion
failure.

## File Structure

All `.fai` files in `src/` form a single module. Split code by concern:

```
src/
  types.fai      — type declarations
  main.fai       — entry point (for runnable projects)
  <name>.fai     — one file per function or logical group
  internal.fai   — private helpers (put private: at top)
```

### Module loading order

Files load alphabetically. This matters for `let` constants — they are NOT
forward-declared, so any file that references a constant must load after it.

Prefix files with `_` to sort them first: `_constants.fai`, `_ffi.fai`.

### `private:` is sticky

`private:` is a mode, not a per-declaration keyword. Once written in a file,
**all subsequent declarations in that file become private**. Keep all public
declarations above any `private:` line.

```fai
# public.fai
def publicFn ...   # ← exported

private:           # ← everything below is private
def helper ...     # ← NOT exported
```

## Writing Code

- One function per file is the preferred style for libraries
- Every function needs at least one test — `fai test` fails otherwise
- Use `test <fnName>` blocks with `it '...'` cases in the same file
- `private:` helpers are covered when their callers are tested
- `print()` is a builtin — no import needed
- String interpolation: `"hello {{name}}"` (double quotes, double braces)
- Arrays: `[1, 2, 3]` (commas required)
- Module functions: `array.length(arr)`, `dictionary.set(d, k, v)`
- Prefer `let` (immutable) over `var`

## Example function with test

```fai
# Compute the area of a rectangle.
def area
    @param width Float
    @param height Float
    @return Float
do
  width * height
end

test area
it 'multiplies width by height'
  assert.equals(area(3.0, 4.0), 12.0)
end
end
```

## Fullstack RPC (multi-target projects)

For projects with `[project.client]` + `[project.server]` in fai.toml:

**Server** (`src/server/main.fai`):
```fai
use std.http.server
use { handleRpcRequest } from Forui.rpc   # required

remote type Task                            # exported to client proxy
  id Int
  text String
end

remote def getTasks                         # exported to client proxy
    @param token String
    @return Task[]
do
  # implementation
end

def main
    @return Void
do
  var r = server.router()
  addRpcRoutes(r)                           # auto-generated — do not define manually
  server.listen(r, 3040)
end
```

**Client** (`src/client/pages/tasks.fai`):
```fai
use { Task, getTasks } from Server        # auto-generated proxy module
use { useSignal, isLoading } from Forui.signal
```

**Rules:**
- Every function the client calls must be `remote def` in the server
- Every type the client uses from the server must be `remote type`
- `addRpcRoutes` is auto-generated — never write it yourself
- `use { handleRpcRequest } from Forui.rpc` is required in the server entry file
- Use `fai build` (no target) to build both client and server at once
- View modifier functions (`fontSize`, `foreground`, `padding`, etc.) must be explicitly imported from `Forui.view` in every file that uses them

## MCP Server

This project ships `.mcp.json` — Claude Code picks it up automatically and can run
`fai_fmt`, `fai_check`, `fai_test`, `fai_run`, `fai_build`, and `fai_doc` as tools.
Start the server manually with `fai mcp` (runs until killed, reads stdin/writes stdout).
