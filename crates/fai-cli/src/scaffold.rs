use crate::templates;

pub(crate) fn cmd_new(args: &[String]) {
    let parsed = match parse_new_args(args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(1);
        }
    };

    let project_root = std::path::Path::new(&parsed.project_dir);

    if project_root.exists() {
        eprintln!("error: target already exists: {}", project_root.display());
        std::process::exit(1);
    }

    let project_name = project_root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    if let Some(tref_str) = &parsed.template {
        let tref = match templates::parse_template_ref(tref_str) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        };
        scaffold_from_template_ref(tref, project_root, &project_name);
        return;
    }

    inline_scaffold(project_root, &project_name);
}

struct NewArgs {
    project_dir: String,
    template: Option<String>,
}

fn parse_new_args(args: &[String]) -> Result<NewArgs, String> {
    let mut positional: Vec<String> = Vec::new();
    let mut template: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--template" => {
                i += 1;
                if i >= args.len() {
                    return Err("error: --template requires a value".to_string());
                }
                template = Some(args[i].clone());
            }
            "--yes" | "-y" => {
                // Reserved for future confirmation prompts (network mode).
                // Currently a no-op for the local-template path.
            }
            arg if arg.starts_with("--") => {
                return Err(format!("error: unknown flag: {}", arg));
            }
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    if positional.is_empty() {
        return Err("Usage: forai new <project-dir> [--template <ref>]".to_string());
    }
    if positional.len() > 1 {
        return Err(format!(
            "error: expected one project directory, got {}",
            positional.len()
        ));
    }
    Ok(NewArgs {
        project_dir: positional.into_iter().next().unwrap(),
        template,
    })
}

fn scaffold_from_template_ref(
    tref: templates::TemplateRef,
    project_root: &std::path::Path,
    project_name: &str,
) {
    match tref {
        templates::TemplateRef::Local(path) => {
            let opts = templates::ScaffoldOptions {
                template_root: &path,
                target_dir: project_root,
                project_name,
            };
            if let Err(e) = templates::scaffold_from_local(&opts) {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
            overlay_meta_files(project_root, project_name);
            println!("scaffolded {} from {}", project_name, path.display());
        }
        templates::TemplateRef::Github {
            owner,
            repo,
            git_ref,
        } => {
            scaffold_from_github(
                &owner,
                &repo,
                git_ref.as_deref(),
                project_root,
                project_name,
            );
        }
        templates::TemplateRef::Url { .. } => {
            eprintln!("error: arbitrary URL templates are not yet supported");
            eprintln!("note: use the GitHub shorthand `<owner>/<repo>[#ref]`");
            std::process::exit(1);
        }
    }
}

#[cfg(feature = "http-client")]
fn scaffold_from_github(
    owner: &str,
    repo: &str,
    git_ref: Option<&str>,
    project_root: &std::path::Path,
    project_name: &str,
) {
    let ref_label = git_ref.unwrap_or("HEAD");
    println!(
        "fetching https://github.com/{}/{} ({})",
        owner, repo, ref_label
    );
    let template_root = match templates::fetch_github_template(owner, repo, git_ref) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    let opts = templates::ScaffoldOptions {
        template_root: &template_root,
        target_dir: project_root,
        project_name,
    };
    let res = templates::scaffold_from_local(&opts);
    let _ = std::fs::remove_dir_all(&template_root);
    if let Err(e) = res {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
    overlay_meta_files(project_root, project_name);
    println!(
        "scaffolded {} from {}/{} ({})",
        project_name, owner, repo, ref_label
    );
}

#[cfg(not(feature = "http-client"))]
fn scaffold_from_github(
    _owner: &str,
    _repo: &str,
    _git_ref: Option<&str>,
    _project_root: &std::path::Path,
    _project_name: &str,
) {
    eprintln!("error: this fai build was compiled without the `http-client` feature");
    eprintln!("note: rebuild with `--features http-client` to use network templates");
    std::process::exit(1);
}

fn inline_scaffold(project_root: &std::path::Path, project_name: &str) {
    let src_dir = project_root.join("src");
    if let Err(e) = std::fs::create_dir_all(&src_dir) {
        eprintln!("error creating directory: {}", e);
        std::process::exit(1);
    }

    let project_files: Vec<(std::path::PathBuf, String)> = vec![
        (src_dir.join("main.fai"), scaffold_main(project_name)),
        (
            project_root.join("fai.toml"),
            scaffold_fai_toml(project_name),
        ),
        (
            project_root.join("README.md"),
            scaffold_readme(project_name),
        ),
    ];

    for (path, content) in &project_files {
        if let Err(e) = std::fs::write(path, content) {
            eprintln!("error writing {}: {}", path.display(), e);
            std::process::exit(1);
        }
    }

    overlay_meta_files(project_root, project_name);

    println!("created project '{}'", project_name);
}

/// Write language-level metadata files (`CLAUDE.md`, `AGENTS.md`,
/// `language.md`, `.mcp.json`, `.codex/config.toml`) into a project
/// directory. These belong with the language tooling, not with any
/// individual template — `fai new` overlays them onto every new
/// project regardless of template source.
///
/// `AGENTS.md` and `CLAUDE.md` are special: when the template ships
/// its own copy, the scaffold's language-level guidance is written
/// first and the template's content is appended below a separator.
/// This keeps language-level rules (doc comments, testing) visible
/// while preserving template-specific guidance the user picked.
///
/// All other files use last-write-wins semantics: a file the template
/// already shipped is left alone; anything missing is filled in.
pub(crate) fn overlay_meta_files(dir: &std::path::Path, project_name: &str) {
    let codex_dir = dir.join(".codex");
    if !codex_dir.exists() {
        let _ = std::fs::create_dir_all(&codex_dir);
    }

    // Append-on-collision: language scaffold + template-shipped content.
    let merging: Vec<(std::path::PathBuf, String)> = vec![
        (dir.join("CLAUDE.md"), scaffold_claude_md(project_name)),
        (dir.join("AGENTS.md"), scaffold_agents_md()),
    ];
    for (path, scaffold) in &merging {
        write_with_template_append(path, scaffold);
    }

    // Fill-only-if-missing: language reference + tool configs.
    let fill_only: Vec<(std::path::PathBuf, String)> = vec![
        (dir.join("language.md"), scaffold_language_md()),
        (dir.join(".mcp.json"), scaffold_mcp_json()),
        (dir.join(".codex/config.toml"), scaffold_codex_config()),
    ];
    for (path, content) in &fill_only {
        if path.exists() {
            continue;
        }
        if let Err(e) = std::fs::write(path, content) {
            eprintln!("warning: could not write {}: {}", path.display(), e);
        }
    }
}

/// Write `scaffold` to `path`. If the template already shipped a file
/// at this path, append its content below a separator so both stay
/// visible. The scaffold goes first because language-level rules
/// (e.g. doc-comment requirement) are universal and should be the
/// first thing an agent reads.
fn write_with_template_append(path: &std::path::Path, scaffold: &str) {
    let template_content = if path.exists() {
        std::fs::read_to_string(path).ok()
    } else {
        None
    };
    let combined = match template_content {
        Some(t) if !t.trim().is_empty() => {
            format!(
                "{}\n---\n\n# Project-specific guidance\n\n{}",
                scaffold.trim_end(),
                t.trim_start()
            )
        }
        _ => scaffold.to_string(),
    };
    if let Err(e) = std::fs::write(path, combined) {
        eprintln!("warning: could not write {}: {}", path.display(), e);
    }
}

pub(crate) fn scaffold_main(project_name: &str) -> String {
    format!(
        r#"# {name} entry point

def main
    @return Void
do
  print('hello from {name}')
end
"#,
        name = project_name
    )
}

pub(crate) fn scaffold_fai_toml(project_name: &str) -> String {
    format!(
        r#"[project]
name = "{name}"
version = "0.1.0"
source_root = "src"

[dependencies]
"#,
        name = project_name
    )
}

pub(crate) fn scaffold_readme(project_name: &str) -> String {
    format!(
        r#"# {name}

A forai project.

## Commands

```bash
fai run        # fmt → check → test → run
fai check      # fmt → check
fai test       # fmt → check → test
fai fmt        # format source files
fai build      # fmt → check → test → build (.wasm)
```
"#,
        name = project_name
    )
}

pub(crate) fn scaffold_language_md() -> String {
    r#"# forai Language Reference

forai is a statically-typed language with strong type inference. Comments start
with `#`. All blocks are `end`-delimited. `print()` is a builtin — no import needed.

## Variables

```fai
let x = 42           # immutable — no reassignment, no field mutation
var count = 0        # mutable — can reassign and mutate fields
let s String = 'hi'  # optional explicit type annotation
let n Int? = null    # optional type (can be null)

var user = User(name: 'Alice', age: 30)
user.age = 31        # OK — var allows field mutation
user.age             # field access
```

All assignments are **deep copies** — variables never share references.

## Functions

Named functions use `@param` / `@return` / `do...end`. A doc comment is required
on every named function except `main`.

```fai
# Add two integers.
def add
    @param a Int
    @param b Int
    @return Int
do
  a + b
end

# Greet by name.
def greet
    @param name String
    @param greeting String, default: 'hello'
    @return String
do
  "{{greeting}}, {{name}}"
end

add(1, 2)                          # positional call
greet(name: 'Alice')               # named call (uses default greeting)
greet('Alice', greeting: 'hey')    # mixed
```

### UFCS — method-style calls

Any function can be called as a method on its first argument:

```fai
5.add(3)            # same as add(5, 3)
label.fontSize(14)  # same as fontSize(label, 14) — must be imported!
```

You can chain UFCS modifiers directly on the result of a `do...end`
trailing-closure call:

```fai
let view = VStack do
    Label('hi')
end.padding(12).background('#fafafa')
```

### Mutable parameters

By default function parameters are immutable copies. Use `mutable` to allow
the function to mutate the caller's binding in place:

```fai
# Increment a counter.
def increment
    @param c Counter, mutable
    @return Void
do
  c.value = c.value + 1
end

var c = Counter(value: 0)
increment(c)     # c.value is now 1 — only var bindings can be passed as mutable
```

### Anonymous closures (`do...end`)

```fai
run(do
  print('hello')
end)

apply(5, do with n Int
  n * 2
end)

# Trailing block — when the last param is a type def:
Button('Click me', onClick: do
  print('clicked')
end)
```

### Generic functions

```fai
# Echo a value.
def echo
    @type T
    @param value T
    @return T
do
  value
end

echo(42)      # T inferred as Int
echo('hi')    # T inferred as String
```

## Types

```fai
type Point
  x Int
  y Int
end

let p = Point(x: 1, y: 2)   # construction uses named args
p.x                           # field access

var q = Point(x: 3, y: 4)
q.x = 10                      # field mutation (var only)
```

### Type-typed fields (callbacks)

```fai
type def ClickAction
    @return Void
end

type Button
  label String
  onClick ClickAction?
end
```

### Generic types

```fai
type Box
  @type T
  value T
end

let b = Box(value: 42)   # T inferred as Int
```

## Enums

```fai
enum Status
  active
  loading
  error
end

let s = Status.active

case s
when Status.active
  print('ok')
when Status.loading
  print('wait')
default
  print('error')
end
```

## Strings

```fai
let plain = 'no interpolation'
let name = 'world'
let msg = "hello {{name}}"    # double-quote + double-brace for interpolation
let joined = 'hello' + ' ' + 'world'
```

## Arrays and Dictionaries

```fai
let nums = [1, 2, 3]          # array literal (commas required)
let first = nums[0]
let count = length(nums)

var list = [1, 2, 3]
list[0] = 99                   # index mutation (var only)

let d = {}                     # empty dict
let d2 = set(d, 'key', 'val') # returns new dict
getString(d2, 'key')           # => 'val' (String?)
getInt(d2, 'num')              # => Int?
getKeys(d2)                    # => String[]
```

## Optionals

```fai
let x Int? = null
if x?              # check: is it non-null?
  print(x!)        # unwrap: force-extract value (panics if null)
end
let safe = unwrap(x, 0)   # unwrap with fallback
```

## Control Flow

```fai
if x > 0
  print('positive')
else if x == 0
  print('zero')
else
  print('negative')
end

var i = 0
while i < 10
  i = i + 1
end

for item in ['a', 'b', 'c']
  print(item)
end

for i in 0..9   # range: 0 inclusive to 9 inclusive
  print(i)
end
```

## Error Handling

```fai
try
  let data = fetchData()
catch e
  print(e.message)
finally
  cleanup()
end

throw Error('something went wrong')
```

## Concurrency

```fai
nowait logEvent('page_viewed')                      # fire and forget
let a, b = all(fetchUsers(), fetchPosts())          # parallel, await both
sleep(500)                                          # pause without blocking host
```

## Modules and Imports

A module is a **directory** of `.fai` files. Import by directory name, not filename.

```fai
# Same project — sibling directory
use { Nav, Section } from client.components    # src/client/components/
use { HomePage } from client.pages             # src/client/pages/
use { isLoggedIn } from client.state           # src/client/state/

# External package (listed in fai.toml [dependencies])
use { mount } from Forui                       # package named "Forui"
use { Label, Button, VStack } from Forui.view  # sub-module Forui/view/
use { useSignal, isLoading, reload } from Forui.signal
use { navigate, Link } from Forui.router

# IMPORTANT: every UFCS function (e.g. label.fontSize(14)) must be explicitly
# imported in the file that uses it — there is no global namespace.
use { fontSize, foreground, padding } from Forui.view   # required per-file

# Cross-target (server importing client for SSR, when both share source = "src")
use { App } from client

# Auto-generated RPC proxy (fullstack projects — see AGENTS.md)
use { Task, getTasks } from Server
```

### Namespace import

```fai
use std.array
array.length([1, 2, 3])   # qualified call

use { length, append } from std.array
length([1, 2, 3])          # unqualified call
```

### Visibility

```fai
def publicFn           # exported by default
    @return Void
do end

private:               # everything below is NOT exported
def helper
    @return Void
do end
```

## Testing

```fai
# Tests live in the same file as the function they test.
# Every function needs at least one test — fai test fails otherwise.

# Add two integers.
def add
    @param a Int
    @param b Int
    @return Int
do
  a + b
end

test add
it 'adds positive numbers'
  assert.equals(add(1, 2), 3)
end
it 'handles negatives'
  assert.equals(add(-1, 1), 0)
end
end
```

## Standard Library

Run `fai doc std` to browse all modules, or `fai doc std.array` for a specific one.
Full signatures and examples are available via `fai doc <name>`.

Key modules: `std.string`, `std.array`, `std.dictionary`, `std.math`, `std.convert`,
`std.json`, `std.http.request`, `std.http.server`, `std.file`, `std.path`,
`std.error`, `std.time`, `std.log`, `std.cli`

```fai
use std.array
use std.string
use std.convert

array.length([1, 2, 3])          # 3
string.contains('hello', 'ell')  # true
toString(42)                     # '42'  (also available as builtin)
parseInt('42')                   # 42   (throws on invalid input)
```
"#
    .to_string()
}

pub(crate) fn scaffold_claude_md(project_name: &str) -> String {
    format!(
        r#"# {name}

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
use {{ HomePage }} from client.pages          # src/client/pages/*.fai
use {{ Label, Button, fontSize }} from Forui.view  # must import each UFCS function
```

**Testing** — required for every public function or `fai test` fails.
**Types** — `type Task {{ id Int\n  text String\nend }}` · constructed with named args · `Task(id: 1, text: 'x')`.
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
- String interpolation: `"hello {{{{name}}}}"` (double quotes, double braces)
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
use {{ handleRpcRequest }} from Forui.rpc   # required

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
use {{ Task, getTasks }} from Server        # auto-generated proxy module
use {{ useSignal, isLoading }} from Forui.signal
```

**Rules:**
- Every function the client calls must be `remote def` in the server
- Every type the client uses from the server must be `remote type`
- `addRpcRoutes` is auto-generated — never write it yourself
- `use {{ handleRpcRequest }} from Forui.rpc` is required in the server entry file
- Use `fai build` (no target) to build both client and server at once
- View modifier functions (`fontSize`, `foreground`, `padding`, etc.) must be explicitly imported from `Forui.view` in every file that uses them

## MCP Server

This project ships `.mcp.json` — Claude Code picks it up automatically and can run
`fai_fmt`, `fai_check`, `fai_test`, `fai_run`, `fai_build`, and `fai_doc` as tools.
Start the server manually with `fai mcp` (runs until killed, reads stdin/writes stdout).
"#,
        name = project_name
    )
}

pub(crate) fn scaffold_agents_md() -> String {
    r#"# AGENTS.md

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
"#
    .to_string()
}

fn scaffold_mcp_json() -> String {
    r#"{
  "mcpServers": {
    "fai": {
      "command": "fai",
      "args": ["mcp"]
    }
  }
}
"#
    .to_string()
}

fn scaffold_codex_config() -> String {
    r#"[mcp_servers.fai]
command = "fai"
args = ["mcp"]
enabled = true
startup_timeout_sec = 10
tool_timeout_sec = 120
"#
    .to_string()
}
