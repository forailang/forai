# AGENTS.md

Guidelines for AI agents working on this forai project.

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

**fai.toml** — project config (name, version, source_root, dependencies, targets).
Call `fai_examples` with query "fai.toml" for a complete template. Call `fai doc lang` for the full reference.

**Functions** — require doc comment + test block. `fai test` fails if any function is uncovered:
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
it 'multiplies dimensions'
  assert.equals(area(3.0, 4.0), 12.0)
end
end
```

**Modules** — a directory is a module. Import by directory name, not filename:
```fai
use { HomePage } from client.pages        # src/client/pages/*.fai → client.pages module
use { Label, Button, fontSize } from Forui.view  # every UFCS fn must be explicitly imported
```

**Types** — struct-like, construction uses named args, field access with dot notation:
```fai
type Task
  id Int
  text String
  done Bool
end
let t = Task(id: 1, text: 'hello', done: false)
t.text                   # field access
```
Built-ins: `Int`, `Float`, `String`, `Bool`, `T[]`, `T?`.
Arrays: `[1, 2, 3]` (commas required). Dicts: `getString(d,'k')→String?`, `getInt(d,'k')→Int?`.
Optionals: `x?` checks · `x!` unwraps · `unwrap(x, fallback)` safe.

## CLI Commands

```bash
fai fmt            # format src/
fai check          # fmt → type-check
fai test           # fmt → check → run tests (REQUIRED — missing test = failed test)
fai run            # fmt → check → test → run
fai build          # fmt → check → test → build (no target = all targets)
fai doc <query>    # look up docs: 'lang', 'std.array', 'Signal', 'Forui.view'
# add -v / --verbose to any of the above for per-file details.
```

## MCP Tools (when using fai mcp)

- `fai_doc query:"Signal"` — find type/function docs
- `fai_doc query:"lang.modules"` — import patterns
- `fai_examples query:"rpc"` — complete RPC server+client example
- `fai_examples query:"fai.toml"` — project config template
- `fai_examples query:"http"` — HTTP+JSON fetch pattern
- `fai_examples query:"ui"` — UI component testing with testMount
- `fai_examples query:"children"` — custom component that takes a do...end block

## File Structure

```
src/
  types.fai      — type declarations
  main.fai       — entry point (runnable projects only)
  <name>.fai     — one file per function or logical group
  internal.fai   — private helpers (private: at the top)
```

Files load alphabetically. `let` constants are NOT forward-declared — files referencing
a constant must load after it. Prefix with `_` to force early load: `_constants.fai`.

### `private:` is sticky

Once `private:` appears in a file, ALL declarations below it in that file
become private. Keep all public declarations ABOVE any `private:` line.

## Writing Code

- Every function needs a test — `fai test` is a hard failure otherwise
- Write `test <fnName>` blocks in the same file as the function
- `private:` helpers are tested via their callers
- Function syntax: `def name\n    @param x Type\n    @return Type\ndo\n  body\nend`
- String interpolation: `"hello {{name}}"` — double quotes and double braces
- Arrays: `[1, 2, 3]` — commas required
- Prefer `let` (immutable) over `var`

## Fullstack RPC (multi-target projects)

For projects with `[project.client]` + `[project.server]` in fai.toml:

**Server** (`src/server/main.fai`):
```fai
use std.http.server
use { handleRpcRequest } from Forui.rpc   # required

remote type Task                          # exported to client proxy
  id Int
  text String
end

remote def getTasks                       # exported to client proxy
    @param token String
    @return Task[]
do
  # implementation
end

def main
    @return Void
do
  var r = server.router()
  addRpcRoutes(r)                         # auto-generated — do not define manually
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
- Use `fai_build` (no target) to build both client and server at once

## Common Mistakes

- **One big file** — split by function/concern, not everything in main.fai
- **Missing tests** — every function needs at least one test or the pipeline fails
- **`private:` placement** — public declarations must come BEFORE `private:` in a file
- **Wrong string syntax** — interpolation uses `"{{var}}"` not `'{{var}}'`
- **Arrays without commas** — use `[1, 2, 3]` not `[1 2 3]`
- **No import for print** — `print()` is a builtin, never import it
- **Missing modifier imports** — `fontSize`, `foreground`, `padding` etc. must be imported from `Forui.view` in every file that uses them
- **`remote def` missing** — if the client's `from Server` import is empty, the server functions are not marked `remote def`
- **`fai_build` with wrong target** — for fullstack projects, call `fai_build` with no `target` to build all sub-projects; do NOT pass `"wasm"` as the target
- **Custom container parse error** — `Section do ... end` works ONLY when `Section` has a parameter typed `Children` (the closure type). See `fai_examples query:"children"`.
- **Big functions → register overflow** — the compiler errors out when a single function needs more than 256 registers. Split complex UI blocks (e.g. long VStack catalogues) into helper functions that return `ViewNode`.

## MCP Server

This project includes `.mcp.json` (Claude Code) and `.codex/config.toml` (Codex) that
start the fai MCP server automatically. The server exposes all fai CLI commands as MCP
tools so AI agents can run `fai_fmt`, `fai_check`, `fai_test`, `fai_run`, `fai_build`,
`fai_doc`, and `fai_new` directly.

For system-wide setup outside this project, add to your agent's user config:

**Claude Code** (`~/.claude/settings.json`):
```json
{
  "mcpServers": {
    "fai": { "command": "fai", "args": ["mcp"] }
  }
}
```

**Codex** (`~/.codex/config.toml`):
```toml
[mcp_servers.fai]
command = "fai"
args = ["mcp"]
enabled = true
tool_timeout_sec = 120
```
