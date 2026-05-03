# `fai.toml` — Project Configuration Reference

Every forai project is described by a single `fai.toml` file at its
root. The file is plain TOML; the parser is
line-oriented and recognises a fixed set of sections.

This doc is the canonical reference for what each section and key
does. The parsing code lives in `crates/fai-cli/src/lib.rs` —
`parse_project_info` (sections + most keys) and `find_source_root`
(`source_root` is read separately because it's needed earlier in
the pipeline).

## Top-level shape

A `fai.toml` describes one or more buildable **projects**. Two
shapes:

- **Single-project** — one `[project]` table, optionally with
  `[dependencies]`. The common case: a library, a CLI tool, or an
  app that builds to a single artifact.
- **Multi-project** — `[project]` carrying shared identity plus
  one or more `[project.<name>]` tables, each describing a separate
  project's build. Used when the same source tree compiles into
  more than one thing — for example, a fullstack app where a
  browser SPA and a native server are two projects sharing one
  source root.

A "project" is whatever `forai build` will produce one artifact
for. In a multi-project file, `forai build` builds every named
project; `forai build -p <name>` builds just one.

---

## `[project]` and `[project.<name>]` — defining projects

The `[project]` table and any `[project.<name>]` tables are the
same kind of thing — descriptions of a project — at different
scopes. What changes between scopes is which keys are valid and
what they mean.

In a **single-project** file, everything goes in `[project]`. There
is one project; that project's identity (`name`, `version`, …) and
its build (`target`, `build_dir`, …) are all top-level keys.

In a **multi-project** file, the responsibilities split:

- The top-level `[project]` carries the **shared identity** for
  the whole repo: `name`, `version`, `source_root`, `docs`.
  These are values that don't differ between the projects.
- Each `[project.<name>]` describes **one project's build**:
  `target`, `source`, `main`, `build_dir`, `rpc_server`.

Sub-projects do **not** inherit `target` or `build_dir` from the
top-level `[project]` — each sub-project sets them independently.
The top-level `target` and `build_dir` are only consulted in
single-project mode.

In multi-project mode, **each sub-project's build artifact is named
after its section key** — `[project.web]` produces `web.wasm`,
`[project.server]` produces `server.wasm`. The top-level
`[project].name` only names artifacts in single-project mode.

### Quick reference

| Key | `[project]` (single) | `[project]` (multi) | `[project.<name>]` | Type | Default |
|---|---|---|---|---|---|
| `name` | ✓ | ✓ | — | string | `"unknown"` |
| `version` | ✓ | ✓ | — | string | `"0.0.0"` |
| `source_root` | ✓ | ✓ | — | string | `"."` |
| `docs` | ✓ | ✓ | — | string | absent |
| `target` | ✓ | (unused) | ✓ | `wasm` / `wasm-html` / `native` | `"wasm"` |
| `build_dir` | ✓ | (unused) | ✓ | string | target-dependent |
| `source` | (no-op) | (no-op) | ✓ | string | inherits `source_root` |
| `main` | — | — | ✓ | string | auto-discovered |
| `rpc_server` | — | — | ✓ | bool | `false` |

The rest of this section walks each key in detail.

---

### `name`

Top-level only. The project's import name.

```toml
[project]
name = "MySuperApp"
```

Two things flow from `name`:

- **Imports.** Everything that imports your project writes
  `use { X } from <name>`, so this is the value consumers see.
  Convention is PascalCase: `Forui`, `HtmlForui`, `Forsqlite`.
- **Build artifact filename.** `forai build` produces
  `<name>.wasm` (and `<name>` for `target = "native"`). This is
  independent of the entry source file's stem — `name = "MySuperApp"`
  builds to `MySuperApp.wasm` whether the entry is `main.fai`,
  `app.fai`, or anything else.

If `name` is unset (parser leaves it as the default `"unknown"`)
the artifact filename falls back to the entry source file's stem,
so an ad-hoc `forai build foo.fai` against a loose file with no
`fai.toml` still produces `foo.wasm`.

Default `"unknown"` — fine for one-off scratches but you'll want to
set it for anything that ships.

---

### `version`

Top-level only. A semver string, displayed by tooling.

```toml
version = "0.1.0"
```

Currently not validated against anything — `[dependencies]`
specifiers carry their own version strings but those aren't checked
against this either. Treat it as documentation today; it's the field
to bump when you cut a release.

Default `"0.0.0"`.

---

### `source_root`

Top-level only. The directory holding `.fai` source files, relative
to the project root.

```toml
source_root = "src"
```

Default `"."` (source files live next to `fai.toml`). Convention is
`"src"`, which every example and library in this codebase uses.

`source_root` is read by a separate pass than the rest of the keys
(it's needed before the main parser runs), so it must be in the
literal `[project]` section — putting it inside a sub-project has no
effect.

> Sub-projects use `source` (below) to override which directory
> their build pulls from. The top-level `source_root` keeps applying
> elsewhere.

---

### `docs`

Top-level only. Path to a markdown file used as the project overview.

```toml
docs = "docs.md"
```

Running `fai doc <name>` displays this file at the top of the
project's doc page. Optional. Path is relative to the project root.

---

### `target`

Build target. Determines what `fai build` produces. Valid values:

```toml
target = "wasm"        # default
target = "wasm-html"
target = "native"
```

Default is `"wasm"`. Unknown values are silently ignored (the parser
treats them as None, then the build path falls back to the same
default). All three values are described below.

#### `target = "wasm"`

**Pure WebAssembly.** Produces a single `.wasm` file consumable by
any wasm host. The forai CLI runs it through the bundled wasmtime
host runner, which provides the same builtin imports the language
checker assumes (file I/O, HTTP, events, etc.). Use for:

- Libraries (where consumers do the building).
- CLI tools that run via `fai run` or are invoked through wasmtime.
- Server code that reads from `std.http.server` and listens on a
  port. The wasmtime host implements those imports.

Output: `<project-name>.wasm` next to the source by default (or in
`build_dir` if set). The filename comes from `[project].name`, not
from the entry source file's stem — `name = "MyApp"` always builds
to `MyApp.wasm` whether the entry is `main.fai`, `app.fai`, or
anything else.

This is what `fai run main.fai` boils down to: build to wasm, hand
the bytes to the host runner, invoke `_start`.

#### `target = "wasm-html"`

**WebAssembly + browser bundle.** Builds the same wasm, then
generates the surrounding HTML/JS so it can be loaded directly in a
browser. The build emits multiple files:

- `<project-name>.wasm` — the compiled program. Filename comes
  from `[project].name`.
- `index.html` — minimal page that fetches the wasm and runs it.
- `fai-runtime.js` — host shim that implements the env imports
  (storage, fetch, DOM bridge, etc.) in JavaScript.
- `forui.css` — present when forui's view layer is in scope.

Use for browser SPAs and any code that calls `mount(App,
htmlRender)`. The `htmlRender` adapter (from html-forui) only knows
how to talk to the JS shim, so wasm-only builds of forui apps render
to a static string instead of a live DOM tree.

Output directory defaults to `"public"`. Override with `build_dir`.

You can also pass `--html` to `fai build` ad-hoc to switch a wasm
project into wasm-html for a single command. The toml setting is
the durable form.

> The forui CLI today does not include a dev-server step — to view a
> wasm-html build, serve `build_dir` over HTTP yourself
> (`python3 -m http.server`) or run a forai server target that
> serves the dir.

#### `target = "native"`

**Self-extracting native executable with embedded wasm and runtime
baked in.** Single binary you can ship.

Use for distributable CLI tools and servers where you don't want
recipients to install a forai toolchain. The wasmtime host is
embedded; the binary is bigger than the bare `.wasm` (tens of MB)
but runs anywhere the corresponding OS/arch supports.

Built per the host platform — there is no cross-compilation today,
so a `target = "native"` build produces a binary for the OS/arch
the build ran on. The forai CI matrix builds the `fai` binary
across linux/{x86_64,aarch64} and darwin/aarch64 if you need a
reference for what's tested.

Output: `<project-name>` (no extension on Unix). Filename comes
from `[project].name`. Default location next to the source unless
`build_dir` is set.

---

### `build_dir`

Output directory for `fai build` artifacts, relative to the project
root.

```toml
build_dir = "public"
```

Default depends on `target`:

- `wasm`: alongside the source file, no separate dir.
- `wasm-html`: `"public"`.
- `native`: alongside the source file.

Setting `build_dir` always wins over the default. The artifact
filename inside is `<project-name>.wasm` (or `<sub-project-name>.wasm`
for sub-projects). For sub-projects, typical practice is
`"build/<name>"` so each sub-project's outputs are isolated:

```toml
[project.web]
target = "wasm-html"
build_dir = "build/web"

[project.server]
target = "native"
build_dir = "build/server"
```

`fai build -o <path>` overrides `build_dir` at the command line.

---

### `source`

Sub-project-only (no-op at top level — use `source_root` there).
The directory this sub-project's build pulls source from, relative
to the project root.

```toml
[project.web]
source = "src"
```

Lets a multi-project repo keep client and server source under the
same root and have each project compile a different subtree (via
`main`, below) within it. If absent, the project pulls from the
top-level `source_root`.

---

### `main`

Sub-project-only. Explicit entry-point file, relative to the project
root.

```toml
[project.web]
main = "src/platforms/web/main.fai"
```

Without `main`, the CLI auto-discovers a file called `main.fai`
inside the sub-project's `source` directory. Set it explicitly when
you have multiple `main.fai` candidates (e.g. a `platforms/web/`
client and a `platforms/server/` server in the same `src/`).

---

### `rpc_server`

Sub-project-only. Marks this build as the one hosting the RPC
endpoint.

```toml
[project.server]
rpc_server = true
```

When `true`, every `remote def` reachable from this sub-project is
compiled with its real body — the function does what its source
says. The build also wires those defs into a generated dispatcher
served at `POST /fai/rpc`.

When `false` (the default), every reachable `remote def` body is
**rewritten at compile time** to a `remoteCall(url, name, args,
hash)` stub that POSTs to the RPC server. The URL comes from the
matching `[project.<sub>.dependencies.<dep>.remote.<env>]` entry
(see below). This is what keeps client wasm from executing
server-only code (DB writes, secrets reads, etc.) when shipped to a
browser.

In a typical fullstack project: `[project.server]` has
`rpc_server = true`; `[project.web]` (or whatever the client is
called) leaves it unset.

---

### `[project.<sub>.dependencies.<dep>.remote.<env>]` — RPC endpoints

A sub-table of a sub-project. Configures the URL the *client*
sub-project hits when calling a `remote def` defined in the *server*
sub-project.

| Key | Type | Purpose |
|---|---|---|
| `url` | string | HTTP(S) endpoint of the RPC server. Baked into the client wasm via the `remoteCall` rewrite. |

The `<env>` segment names a deployment environment (`dev`, `prod`,
…). The build picks `dev` first, falling back to whatever's defined
if no `dev` entry exists.

```toml
[project.web.dependencies.server.remote.dev]
url = "http://localhost:3040"

[project.web.dependencies.server.remote.prod]
url = "https://api.example.com"
```

The middle segment (`dependencies.server.`) is the dep name as
declared in `[dependencies]` — the client thinks of the server as
just another project it depends on.

---

## `[dependencies]`

External projects this project depends on. The keys are quoted
specifiers; the values are version strings (parsed but currently
not validated against the dep's actual version).

The resolver reads each dep's own `fai.toml`, picks up its
`[project].name` and `[project].source_root`, and registers it under
that name for `use { ... } from <Name>` resolution.

### Accepted specifier forms

| Form | Meaning | Example |
|---|---|---|
| `"file:///<absolute>"` | Absolute path. Three slashes after `file:` because that's the URL form. | `"file:///home/me/code/forui"` |
| `"file://<relative>"` | Relative path, resolved against the directory containing **this** `fai.toml` (not the process's `cwd`). Two slashes. | `"file://../../forui"` |

That's the full set today. Notable absences:

- **No git URLs.** Forms like `"git:..."`, `"github:owner/repo"`, or
  `"https://github.com/..."` are silently ignored. To consume a
  project from git you must clone it locally and point at it with a
  `file://` path.
- **No version-only specs.** `"forui" = "0.1.0"` won't resolve —
  every dep needs a `file://` URI.
- **No registry.** There is no equivalent of crates.io / npm yet.

### Path-form gotchas

- `"file:../forui"` (single colon, no slashes) **does not work** —
  the parser strips a `file://` prefix specifically and falls
  through otherwise. Always use `file://` (two slashes) before the
  path.
- Trailing slashes are tolerated.
- The path must point at the project's **root directory** (the one
  containing the dep's own `fai.toml`), not at its `src/` directory.

**Examples:**
```toml
[dependencies]
# Sibling project in the same monorepo — preferred shape for local work
"file://../../forui" = "0.1.0"
"file://../../html-forui" = "0.1.0"

# Absolute path — works but breaks the moment anything moves
"file:///home/me/projects/forsqlite" = "0.1.0"
```

---

## `[remote-interface]`

Used in fullstack setups to lock the client and server to the same
RPC interface version. Two roles: a server **exposes**, a client
**consumes**.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `expose` | bool (bare `true`/`false`, no quotes) | `false` | When `true`, the build writes `interface.json` + `interface.hash` alongside the build output. Other projects can read this hash to detect drift. |
| `from` | string | absent | Name of the peer project whose `interface.hash` to bake into this build. The hash becomes a generated `apiHash()` constant the client sends with each RPC; the server rejects mismatches. |

> **Heads up:** `expose` is parsed as `v == "true"` against the raw
> trimmed value, so it must be written without quotes. `from` is
> unquoted by the parser, so either `from = "server"` or
> `from = server` works in practice — prefer the quoted form for
> consistency.

**Server side:**
```toml
[project]
name = "MyServer"
version = "0.1.0"
source_root = "src"

[remote-interface]
expose = true
```

**Client side:**
```toml
[project]
name = "MyClient"
version = "0.1.0"
source_root = "src"
target = "wasm-html"

[dependencies]
"file://../my-server" = "0.1.0"

[remote-interface]
from = "MyServer"
```

---

## Putting it together — three full examples

### 1. A library

```toml
# forui/fai.toml
[project]
name = "Forui"
version = "0.1.0"
source_root = "src"
docs = "docs.md"
```

No dependencies, no build target — consumers do the building.

### 2. A browser SPA that depends on a library

```toml
# counter/fai.toml
[project]
name = "counter"
version = "0.1.0"
source_root = "src"
target = "wasm-html"
build_dir = "public"

[dependencies]
"file://../../forui" = "0.1.0"
"file://../../html-forui" = "0.1.0"
```

Run `forai build` to produce `public/counter.wasm` + `public/index.html`
+ runtime/css. The wasm filename comes from `[project].name`, not
the entry's source-file stem.

### 3. A fullstack app with shared source, locked RPC interface

```toml
# todo-fullstack/fai.toml
[project]
name = "TodoFullstack"
version = "0.1.0"
source_root = "src"

[project.web]
target = "wasm-html"
source = "src"
main = "src/platforms/web/main.fai"
build_dir = "build/web"

[project.server]
target = "native"
source = "src"
main = "src/platforms/server/main.fai"
build_dir = "build/server"
rpc_server = true

[project.web.dependencies.server.remote.dev]
url = "http://localhost:3040"

[dependencies]
"file://../../forui" = "0.1.0"
"file://../../html-forui" = "0.1.0"
"file://../../forsqlite" = "0.1.0"

[remote-interface]
expose = true
```

`forai build -p server` builds the server target; `forai build -p
web` builds the SPA. Without `-p`, both build.

---

## Parsing notes (edge cases)

- The parser is line-oriented. `#` starts a comment; blank lines and
  malformed `key = value` lines are silently skipped. Continuation
  lines and inline tables are not supported.
- Boolean keys (`rpc_server`, `expose`) accept bare `true` /
  `false`. `expose` specifically requires it unquoted; other booleans
  tolerate either form.
- TOML array literals are not supported anywhere in current keys.
  (A legacy `[workspace] members = [...]` form was parsed for
  backward compatibility but is no longer the recommended shape.)
- Section nesting beyond `[project.<sub>.dependencies.<dep>.remote.<env>]`
  is not interpreted — keys under deeper sections are dropped.
- The order of sections does not matter. Within a section, the last
  assignment wins for duplicate keys (the parser is just a loop —
  no duplicate-detection diagnostic).
