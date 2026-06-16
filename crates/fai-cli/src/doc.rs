//! `fai doc` — documentation lookup for stdlib, project, and dependency functions.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::Path;

// ── Public types ──────────────────────────────────────────────────────────────

/// Summary of a namespace for directory-listing view.
pub struct NamespaceSummary {
    /// Full namespace path, e.g. `"std.http"`.
    pub path: String,
    /// Total functions under this namespace (including sub-namespaces).
    pub fn_count: usize,
    /// True if the namespace has sub-namespaces (drill down with `fai doc <path>`).
    pub has_children: bool,
}

#[derive(Debug, Clone)]
pub struct DocEntry {
    /// Module or package namespace, e.g. `"std.array"`, `"forui"`, or `""` for project-local.
    pub namespace: String,
    /// Short function name, e.g. `"join"`.
    pub name: String,
    /// Full dotted path, e.g. `"std.array.join"` or just `"myFn"` for project-local.
    pub full_path: String,
    /// Rendered signature, e.g. `"join(items: String[], separator: String) -> String"`.
    pub signature: String,
    /// One-or-more-line description.
    pub doc: String,
    /// Where this entry came from.
    pub source: DocSource,
    /// What kind of entity this entry describes. Drives list-view grouping
    /// and detail-view rendering (functions get a def…end block, types get
    /// a type…end block, etc.).
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
    Function,
    Type,
    Enum,
    /// Language reference topics (`lang.rpc.rpc_full_example` etc.). Keeps
    /// the legacy rendering because the body is already self-contained
    /// prose with fenced code blocks.
    LanguageTopic,
    /// A dependency package overview (`Forui`, `HtmlForui`). Whole-body prose.
    PackageOverview,
}

#[derive(Debug, Clone)]
pub enum DocSource {
    Stdlib,
    Project,
    Dependency(String),
    Language,
    PackageOverview(String),
}

// ── Collection ────────────────────────────────────────────────────────────────

/// Collect documentation entries for all stdlib builtins.
pub fn collect_stdlib_docs() -> Vec<DocEntry> {
    use fai_checker::builtins::{all_builtin_docs, install_builtins};
    use fai_checker::types::Type;

    let type_map = install_builtins();
    let mut entries = Vec::new();

    for overview in stdlib_module_overviews() {
        entries.push(DocEntry {
            namespace: "std".to_string(),
            name: overview.name.to_string(),
            full_path: format!("std.{}", overview.name),
            signature: String::new(),
            doc: overview.doc.trim().to_string(),
            source: DocSource::Stdlib,
            kind: EntryKind::PackageOverview,
        });
    }

    for doc in all_builtin_docs() {
        if let Some(Type::Function(sig)) = type_map.get(doc.builtin_name) {
            let signature = render_builtin_sig(doc.name, sig);
            entries.push(DocEntry {
                namespace: doc.module.to_string(),
                name: doc.name.to_string(),
                full_path: format!("{}.{}", doc.module, doc.name),
                signature,
                doc: doc.doc.to_string(),
                source: DocSource::Stdlib,
                kind: EntryKind::Function,
            });
        }
    }

    entries
}

struct StdlibModuleOverview {
    name: &'static str,
    doc: &'static str,
}

fn stdlib_module_overviews() -> &'static [StdlibModuleOverview] {
    &[
        StdlibModuleOverview {
            name: "array",
            doc: r#"
`std.array` contains helpers for reading, transforming, and building arrays.
Most helpers return a new array or value; `append(items, item)` returns the
array with the item added, so assign the result when you want to keep it.

```fai
use std.array

let numbers = [1 2 3 4]

let doubled = array.map(numbers) do with n Int
    n * 2
end

let even = array.filter(numbers) do with n Int
    n % 2 == 0
end

let firstLarge = array.find(numbers) do with n Int
    n > 2
end

let more = array.append(numbers, 5)
let middle = array.slice(numbers, 1, 3)
```

`slice(items, start, end)` uses an exclusive end index. `find`, `first`, and
`last` return optional values, so check with `?` or unwrap before use.
"#,
        },
        StdlibModuleOverview {
            name: "dictionary",
            doc: r#"
`std.dictionary` reads and updates dictionary values. Use typed getters when you
know the expected value type, and use `get` when the value may be any type.

```fai
use std.dictionary

var user = {
    name: 'Ada'
    age: 37
}

user = dictionary.set(user, 'active', true)

let name = dictionary.getString(user, 'name')!
let age = dictionary.getInt(user, 'age')!
let active = dictionary.getBool(user, 'active')!
```

`set` returns an updated dictionary. Assign it back when you want to keep the
change.

Dictionary field syntax is convenient for fixed identifier keys, for example
`user.name`. Use `get`, `getString`, `getInt`, `getBool`, and `set` for dynamic
keys, keys with punctuation such as `'x-api-key'`, or data parsed from JSON.
"#,
        },
        StdlibModuleOverview {
            name: "string",
            doc: r#"
`std.string` contains string search, splitting, joining, slicing, replacement,
case conversion, and trimming helpers.

```fai
use std.string

let raw = '  ada,lovelace  '
let clean = string.trim(raw)
let parts = string.split(clean, ',')
let display = string.join([
    string.toUpper(parts[0])
    string.toLower(parts[1])
], ' ')

let first = string.substring(display, 0, 3)
let updated = string.replace(display, 'ADA', 'Ada')
```

`substring(text, start, end)` uses an exclusive end index. `indexOf` returns the
first matching index or `-1` when the search text is not found.
"#,
        },
        StdlibModuleOverview {
            name: "convert",
            doc: r#"
`std.convert` converts unknown or loosely typed values into common scalar types.
Use it at boundaries such as environment variables, route params, form values,
and parsed JSON.

```fai
use std.convert

let port = convert.parseInt('3040')
let ratio = convert.parseFloat('0.75')
let label = convert.toString(port)
```

`parseInt` and `parseFloat` parse strings and return `null` for invalid input.
Guard the result before using it in code that might receive bad data.

```fai
let id = convert.parseInt(rawId)
if id?
    loadUser(id!)
else
    print('invalid id')
end
```

`toInt`, `toFloat`, `toBool`, and `toString` coerce existing values. Prefer the
parse helpers when converting user-provided strings where invalid input is
expected.
"#,
        },
        StdlibModuleOverview {
            name: "json",
            doc: r#"
`std.json` parses and stringifies JSON. `json.parse` returns `Unknown`, usually
a dictionary for JSON objects and an array for JSON arrays. Reconstruct typed
records manually using dictionary accessors.

```fai
use std.json
use std.dictionary

type User
    id Int
    name String
end

let parsed = json.parse('{"id":1,"name":"Ada"}')
let user = User(
    id: dictionary.getInt(parsed, 'id')!,
    name: dictionary.getString(parsed, 'name')!
)
```

`json.stringify(value)` serializes Forai values back to JSON. `requireString`
is a small convenience for extracting a required string field from a dictionary;
it returns `String?`, so handle the missing case.
"#,
        },
        StdlibModuleOverview {
            name: "http",
            doc: r#"
`std.http` is split into client request helpers and server router helpers:

- `std.http.request` sends HTTP requests and returns `Response` dictionaries.
- `std.http.server` builds responses and runs a small router.

Use `fai doc std.http.request` and `fai doc std.http.server` for the concrete
APIs and examples.
"#,
        },
        StdlibModuleOverview {
            name: "http.request",
            doc: r#"
`std.http.request` sends synchronous HTTP requests. Each helper returns a
`Response` shaped like a dictionary with `status`, `body`, and `headers`.

```fai
use std.http.request
use std.dictionary
use std.json

var headers = {}
headers = dictionary.set(headers, 'accept', 'application/json')
headers = dictionary.set(headers, 'x-api-key', apiKey)

let res = request.get('https://api.example.com/users/1', headers)
if res.status == 200
    let body = json.parse(res.body)
    let name = dictionary.getString(body, 'name')!
end
```

For JSON POST/PUT/PATCH requests, stringify the body yourself and pass a
content-type header.

```fai
var headers = {}
headers = dictionary.set(headers, 'content-type', 'application/json')

let payload = json.stringify({ name: 'Ada' })
let res = request.post('https://api.example.com/users', payload, headers)
```

Transport failures return `null` in current WASM/native host paths rather than
a `Response`, so guard when calling unreliable networks. HTTP error statuses
still return a response with the server status and body.
"#,
        },
        StdlibModuleOverview {
            name: "http.server",
            doc: r#"
`std.http.server` creates a router, registers route handlers, builds responses,
serves static files, and starts listening on a port.

```fai
use std.http.server

def main
    @return Void
do
    var r = server.router()

    server.get(r, '/') do with req HttpRequest
        server.html(200, '<h1>Hello</h1>')
    end

    server.post(r, '/api/echo') do with req HttpRequest
        server.json(200, { body: req.body })
    end

    server.serveFiles(r, 'build/web')
    server.listen(r, 3040)
end
```

Route handlers receive an `HttpRequest` with fields such as `method`, `path`,
`body`, `headers`, and cookies, and must return an `HttpResponse`. Use
`server.text`, `server.html`, `server.json`, `server.redirect`, or `server.ok`
to build responses.

`server.listen` blocks the current program. If a port cannot be bound, the host
prints an error and returns from the listen call.
"#,
        },
        StdlibModuleOverview {
            name: "net",
            doc: r#"
`std.net` contains low-level networking helpers. Use `net.available()` to check
whether the current runtime reports networking support, then use the TCP or UDP
submodules for socket work.

```fai
use std.net

if net.available()
    print('networking is available')
end
```

Use `std.http.request` for normal HTTP calls. Use `std.net.tcp` and
`std.net.udp` only when you need raw socket protocols.
"#,
        },
        StdlibModuleOverview {
            name: "net.tcp",
            doc: r#"
`std.net.tcp` exposes raw TCP listener and connection handles. Handle-returning
functions return an `Int`; close every listener or connection handle when done.

```fai
use std.net.tcp
use std.dictionary

let conn = tcp.connect('127.0.0.1', 9000)
if conn >= 0
    tcp.write(conn, 'ping\n')
    let line = tcp.readLine(conn)
    tcp.close(conn)
end
```

Server-side TCP accepts return a dictionary containing `handle` and `address`:

```fai
let listener = tcp.listen(9000)
let accepted = tcp.accept(listener)
let conn = dictionary.getInt(accepted, 'handle')!
let addr = dictionary.getString(accepted, 'address')!
tcp.write(conn, 'hello ' + addr + '\n')
tcp.close(conn)
tcp.close(listener)
```

In the WASM host path, `listen`, `connect`, and `write` return `-1` on failure;
`accept`, `read`, `readLine`, and `address` return `null` on failure; `close`
returns `Void` and ignores invalid handles.
"#,
        },
        StdlibModuleOverview {
            name: "net.udp",
            doc: r#"
`std.net.udp` exposes raw UDP socket handles. Bind a socket, send datagrams,
receive dictionaries, and close the socket when done.

```fai
use std.net.udp
use std.dictionary

let sock = udp.bind(9001)
if sock >= 0
    udp.send(sock, '127.0.0.1', 9002, 'hello')
    udp.close(sock)
end
```

`udp.receive(socket)` returns a dictionary with `data`, `host`, and `port`:

```fai
let packet = udp.receive(sock)
let data = dictionary.getString(packet, 'data')!
let host = dictionary.getString(packet, 'host')!
let port = dictionary.getInt(packet, 'port')!
```

In the WASM host path, `bind` and `send` return `-1` on failure, `receive`
returns `null` on failure, and `close`/`broadcast` return `Void` while ignoring
invalid handles. Enable broadcast with `udp.broadcast(sock, true)` before
sending to a broadcast address.
"#,
        },
        StdlibModuleOverview {
            name: "env",
            doc: r#"
`std.env` reads process environment variables and can load dotenv-style files.
Call `env.load(path)` before `env.get(key)` when using a local `.env` file.

```fai
use std.env
use std.convert

env.load('.env.dev')

let port = if env.get('SERVER_PORT')?
    convert.parseInt(env.get('SERVER_PORT')!)
else
    3040
end
```

`env.get` returns `String?`. `env.load` returns `true` when the file was read
and merged, or `false` when it is missing or unreadable. Browser builds use
stubs, so server/native code should own environment-dependent behavior.
"#,
        },
        StdlibModuleOverview {
            name: "file",
            doc: r#"
`std.file` reads, writes, checks, and lists files relative to the process
working directory unless you pass an absolute path.

```fai
use std.file

if file.exists('data/config.json')
    let raw = file.read('data/config.json')
    print(raw)
end

let ok = file.write('/tmp/report.txt', 'done\n')
let names = file.list('data')
```

`file.read` returns the file contents as a string. `file.write` returns `Bool`
for success. `file.list` returns entry names in the directory; join them with
`std.path.join` when you need full paths.
"#,
        },
        StdlibModuleOverview {
            name: "process",
            doc: r#"
`std.process` runs shell commands and manages long-running command sessions.
Native-only: `process.available()` returns `false` in browser builds, where
the other functions are not linked. Commands run via `bash -lc`, so pipes,
globs, and env expansion work.

```fai
use std.process
use std.json

if process.available()
    let raw = process.run('ls -la', '.', '{}', 5000, 65536)
    let result Dictionary = json.parse(raw)
    if getBool(result, 'ok')!
        print(getString(result, 'stdout')!)
    end
end
```

`process.run(command, cwd, envJson, timeoutMs, maxOutputBytes)` blocks until
the command exits or the timeout kills it, and returns a JSON string:
`{ok, command, cwd, exitCode, stdout, stderr, timedOut, durationMs,
truncated}`. `ok` is true only for a zero exit without timeout; `exitCode`
is null when the process was killed. An empty `cwd` inherits the host
working directory. `envJson` is a JSON object of extra environment
variables. `timeoutMs` is clamped to 30000 and `maxOutputBytes` to 65536;
zero or negative values select those maximums.

Sessions keep a command running across calls:
`process.start(command, cwd, envJson, lifetimeMs)` returns
`{ok, sessionId, ...}`; `process.write(sessionId, input)` sends stdin;
`process.read(sessionId, maxOutputBytes)` drains buffered output and
reports `{running, exitCode, stdout, stderr, ...}`;
`process.stop(sessionId)` kills and removes the session. Sessions expire
after `lifetimeMs` (clamped to 600000) and are cleaned up lazily.
"#,
        },
        StdlibModuleOverview {
            name: "path",
            doc: r#"
`std.path` provides small path string helpers. Use it with `std.file` when
building paths from directory and filename pieces.

```fai
use std.path

let full = path.join('data', 'users.json')
let dir = path.dirname(full)
let name = path.basename(full)
let ext = path.extname(full)
```

These helpers operate on path strings; they do not check whether the path
exists. Use `std.file.exists` when you need filesystem state.
"#,
        },
        StdlibModuleOverview {
            name: "storage",
            doc: r#"
`std.storage` is a simple string key/value store. Browser builds use
`localStorage`; native/test host paths use a process-local store.

```fai
use std.storage

storage.storageSet('theme', 'dark')

let theme = storage.storageGet('theme')
if theme?
    print('theme: ' + theme!)
end

storage.storageRemove('theme')
```

Values are strings. Use `std.json.stringify` before storing structured data and
`std.json.parse` after reading it. `storageClear()` removes all entries in the
current store and is useful for tests.
"#,
        },
        StdlibModuleOverview {
            name: "browser",
            doc: r#"
`std.browser` exposes browser/runtime bridge functions used by adapters such as
`HtmlForui`. Most app code should use Forui and HtmlForui APIs instead of
calling this module directly.

```fai
use std.browser

let path = browser.getLocationPath()
browser.pushHistoryState('/settings')
browser.replaceLocation('/chat')
browser.setHtmlAt('#app', '<p>Updated</p>')
```

`setHtml` and `setHtmlAt` replace DOM content. `getLocationPath` and
`pushHistoryState` back router integration. `replaceLocation` performs a full
document navigation. `remoteCall` is the low-level RPC transport used by
generated remote stubs.
"#,
        },
        StdlibModuleOverview {
            name: "events",
            doc: r#"
`std.events` is a synchronous in-process event bus. Register handlers with
`on`, emit events with `emit`, and cancel handlers with `off`.

```fai
use std.events

let sub = events.on('task:created') do with e Event
    print('created: ' + toString(e.data))
end

events.emit('task:created', { id: 1 })
events.off(sub)
```

`once` registers a handler that removes itself after the first event.
`subscribers(name)` returns the current active count. `clear(name)` and
`clearAll()` are useful for test cleanup. Handlers run synchronously in
registration order against a snapshot of subscribers.
"#,
        },
        StdlibModuleOverview {
            name: "time",
            doc: r#"
`std.time` reads the current clock.

```fai
use std.time

let startedMs = time.now()
let startedSec = time.unix()
```

`time.now()` returns a `Float` milliseconds-since-Unix-epoch timestamp.
`time.unix()` returns whole Unix seconds as `Int`.
"#,
        },
        StdlibModuleOverview {
            name: "cli",
            doc: r#"
`std.cli` contains terminal input and output helpers for command-line programs.

```fai
use std.cli

let name = cli.readLine('Name: ')
cli.writeLine('Hello, ' + name)
```

`write` prints without a trailing newline. `writeLine` appends a newline.
`clear` clears the terminal screen, and `moveTo(row, column)` moves the cursor
for simple terminal UIs.
"#,
        },
        StdlibModuleOverview {
            name: "log",
            doc: r#"
`std.log` writes leveled log messages through the host runtime.

```fai
use std.log

log.info('server started')
log.warn('cache miss')
log.error('request failed')
```

The functions accept `Unknown`, so strings, numbers, dictionaries, and other
values can be logged without converting first.
"#,
        },
        StdlibModuleOverview {
            name: "ffi",
            doc: r#"
`std.ffi` currently exposes availability checks for native libraries. Use it
before calling code that depends on a system C library.

```fai
use std.ffi

if ffi.available('sqlite3')
    print('sqlite is available')
else
    print('sqlite is missing')
end
```

Native host paths check pkg-config and common library directories. Browser
builds report unavailable.
"#,
        },
        StdlibModuleOverview {
            name: "math",
            doc: r#"
`std.math` provides basic numeric helpers. Most functions operate on `Float`;
rounding helpers return `Int`.

```fai
use std.math

let n = math.random()
let rounded = math.round(n * 100.0)
let root = math.sqrt(81.0)
let clamped = math.max(0.0, math.min(1.0, n))
```

`random()` returns a `Float` from `0.0` up to but not including `1.0`.
`floor`, `ceil`, and `round` return integer values.
"#,
        },
        StdlibModuleOverview {
            name: "error",
            doc: r#"
`std.error` creates and inspects Error values. Use `throw Error(message)` for
explicit failures and `try ... catch` to recover.

```fai
use std.error

try
    throw Error('missing user')
catch e
    print(error.message(e))
end
```

`isError(value)` checks whether an unknown value is an Error. `message(err)` and
`kind(err)` read fields from an Error. `unwrap(value, fallback)` returns
`fallback` when the value is `null` or an Error; otherwise it returns the value.
"#,
        },
        StdlibModuleOverview {
            name: "io",
            doc: r#"
`std.io` currently exposes `print`, the simplest stdout helper.

```fai
use std.io

io.print('hello')
io.print({ status: 'ok' })
```

For command-line programs that need prompts, no-newline writes, cursor movement,
or screen clearing, use `std.cli`.
"#,
        },
        StdlibModuleOverview {
            name: "html",
            doc: r#"
`std.html` provides small HTML string helpers.

```fai
use std.html

let safe = html.escape(userInput)
```

Use `escape` before embedding untrusted text in hand-built HTML strings. For
Forui apps, prefer normal `Forui.view` components and `HtmlForui` rendering so
the adapter owns HTML generation.
"#,
        },
        StdlibModuleOverview {
            name: "test",
            doc: r#"
`std.test` contains the assertion primitives behind test blocks. In normal test
code, prefer the built-in `assert` namespace methods.

```fai
test mathExample
    it 'adds numbers'
        assert.equals(2 + 2, 4)
        assert.isTrue(4 > 2)
    end
end
```

The exported `std.test.assert` and `std.test.equal` functions are low-level
forms. Test blocks automatically provide the more ergonomic `assert.equals`,
`assert.isTrue`, `assert.isFalse`, `assert.isNull`, and related helpers.
"#,
        },
    ]
}

/// Collect documentation entries from all public functions in a project source directory.
pub fn collect_project_docs(source_root: &Path) -> Vec<DocEntry> {
    collect_docs_recursive(source_root, "", DocSource::Project)
}

/// Collect documentation entries from a dependency package.
/// `dep_path` is the dependency's project root (containing `fai.toml`).
/// `dep_name` is the package name used as the namespace prefix.
/// Recursively scans sub-directories as sub-modules (e.g. `forui/src/signal/`
/// becomes the `Forui.signal` namespace).
pub fn collect_dependency_docs(dep_path: &Path, dep_name: &str) -> Vec<DocEntry> {
    let src_dir = dep_source_dir(dep_path);
    let full_src = dep_path.join(&src_dir);
    collect_docs_recursive(
        &full_src,
        dep_name,
        DocSource::Dependency(dep_name.to_string()),
    )
}

/// Collect `docs.md` overview entries from dependency source subdirectories.
///
/// A dependency can add `src/view/docs.md`; it will render as `fai doc Forui.view`
/// before the declaration list for that namespace.
pub fn collect_dependency_module_overviews(dep_path: &Path, dep_name: &str) -> Vec<DocEntry> {
    let src_dir = dep_source_dir(dep_path);
    let full_src = dep_path.join(&src_dir);
    collect_module_overviews_recursive(
        &full_src,
        dep_name,
        dep_name,
        DocSource::PackageOverview(dep_name.to_string()),
    )
}

/// Recursively collect docs from a directory and all its sub-directories.
/// Each sub-directory becomes a child namespace: `<namespace>.<dirname>`.
fn collect_docs_recursive(dir: &Path, namespace: &str, source: DocSource) -> Vec<DocEntry> {
    let mut entries = collect_docs_from_dir(dir, namespace, source.clone());

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return entries;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Skip hidden directories.
            if dir_name.starts_with('.') {
                continue;
            }
            let child_ns = if namespace.is_empty() {
                dir_name.to_string()
            } else {
                format!("{}.{}", namespace, dir_name)
            };
            entries.extend(collect_docs_recursive(&path, &child_ns, source.clone()));
        }
    }

    entries
}

fn dep_source_dir(dep_root: &Path) -> String {
    let toml_path = dep_root.join("fai.toml");
    let Ok(content) = std::fs::read_to_string(&toml_path) else {
        return "src".to_string();
    };
    let mut in_project = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_project = t == "[project]";
            continue;
        }
        if !in_project {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            if k.trim() == "source_root" || k.trim() == "source" {
                return v.trim().trim_matches('"').to_string();
            }
        }
    }
    "src".to_string()
}

fn collect_docs_from_dir(dir: &Path, namespace: &str, source: DocSource) -> Vec<DocEntry> {
    let mut entries = Vec::new();

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return entries;
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().map_or(false, |e| e == "fai") {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(program) = fai_parser::parse(&content) else {
                continue;
            };

            for stmt in &program.statements {
                match stmt {
                    fai_parser::ast::Statement::Function(fd) => {
                        if fd.is_private || fd.name == "main" || fd.name.starts_with("doc_") {
                            continue;
                        }
                        let Some(ref doc_text) = fd.doc_comment else {
                            continue;
                        };

                        let signature = render_parser_fn_sig(fd);
                        let full_path = if namespace.is_empty() {
                            fd.name.clone()
                        } else {
                            format!("{}.{}", namespace, fd.name)
                        };

                        entries.push(DocEntry {
                            namespace: namespace.to_string(),
                            name: fd.name.clone(),
                            full_path,
                            signature,
                            doc: doc_text.clone(),
                            source: source.clone(),
                            kind: EntryKind::Function,
                        });
                    }

                    fai_parser::ast::Statement::Type(td) => {
                        if td.is_private {
                            continue;
                        }
                        // Skip types with no fields — they're likely stubs or aliases.
                        // Always include remote types (RPC surface).
                        if td.fields.is_empty() && !td.is_remote {
                            continue;
                        }
                        let signature = render_type_sig(td);
                        let remote_note = if td.is_remote {
                            "Remote type — exported in the RPC proxy. Accessible via `from Server` on the client.".to_string()
                        } else {
                            String::new()
                        };
                        let full_path = if namespace.is_empty() {
                            td.name.clone()
                        } else {
                            format!("{}.{}", namespace, td.name)
                        };
                        entries.push(DocEntry {
                            namespace: namespace.to_string(),
                            name: td.name.clone(),
                            full_path,
                            signature,
                            doc: remote_note,
                            source: source.clone(),
                            kind: EntryKind::Type,
                        });
                    }

                    fai_parser::ast::Statement::Enum(ed) => {
                        if ed.is_private || ed.members.is_empty() {
                            continue;
                        }
                        let signature =
                            format!("enum {} {{ {} }}", ed.name, ed.members.join(" | "));
                        let full_path = if namespace.is_empty() {
                            ed.name.clone()
                        } else {
                            format!("{}.{}", namespace, ed.name)
                        };
                        entries.push(DocEntry {
                            namespace: namespace.to_string(),
                            name: ed.name.clone(),
                            full_path,
                            signature,
                            doc: String::new(),
                            source: source.clone(),
                            kind: EntryKind::Enum,
                        });
                    }

                    _ => {}
                }
            }
        }
    }

    entries
}

fn collect_module_overviews_recursive(
    dir: &Path,
    namespace: &str,
    dep_name: &str,
    source: DocSource,
) -> Vec<DocEntry> {
    let mut entries = Vec::new();

    let docs_path = dir.join("docs.md");
    if namespace != dep_name {
        if let Ok(text) = std::fs::read_to_string(&docs_path) {
            let parent = namespace
                .rsplit_once('.')
                .map(|(parent, _)| parent.to_string())
                .unwrap_or_default();
            let name = namespace
                .rsplit('.')
                .next()
                .unwrap_or(namespace)
                .to_string();
            entries.push(DocEntry {
                namespace: parent,
                name,
                full_path: namespace.to_string(),
                signature: String::new(),
                doc: text.trim().to_string(),
                source: source.clone(),
                kind: EntryKind::PackageOverview,
            });
        }
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return entries;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if dir_name.starts_with('.') {
                continue;
            }
            let child_ns = format!("{}.{}", namespace, dir_name);
            entries.extend(collect_module_overviews_recursive(
                &path,
                &child_ns,
                dep_name,
                source.clone(),
            ));
        }
    }

    entries
}

/// Render a type declaration as a compact signature showing all fields.
fn render_type_sig(td: &fai_parser::ast::TypeDeclaration) -> String {
    // Always multi-line `type Name\n  fields\nend` — this is used as the
    // detail-view code block. The list view strips everything after the
    // first line, so it renders as just `type Name` in listings.
    if td.fields.is_empty() {
        return format!("type {}\nend", td.name);
    }
    let fields: Vec<String> = td
        .fields
        .iter()
        .map(|f| format!("  {} {}", f.name, ast_type_str(&f.type_node)))
        .collect();
    format!("type {}\n{}\nend", td.name, fields.join("\n"))
}

// ── Language docs ─────────────────────────────────────────────────────────────

/// Collect built-in language reference docs from the embedded doc files.
/// Namespace: `lang.<topic>`, e.g. `lang.variables`, `lang.modules`.
pub fn collect_lang_docs() -> Vec<DocEntry> {
    let files: &[(&str, &str)] = &[
        ("variables", include_str!("../docs/lang/variables.fai")),
        ("functions", include_str!("../docs/lang/functions.fai")),
        ("types", include_str!("../docs/lang/types.fai")),
        ("collections", include_str!("../docs/lang/collections.fai")),
        ("strings", include_str!("../docs/lang/strings.fai")),
        ("modules", include_str!("../docs/lang/modules.fai")),
        (
            "control_flow",
            include_str!("../docs/lang/control_flow.fai"),
        ),
        ("errors", include_str!("../docs/lang/errors.fai")),
        ("concurrency", include_str!("../docs/lang/concurrency.fai")),
        ("testing", include_str!("../docs/lang/testing.fai")),
        ("enums", include_str!("../docs/lang/enums.fai")),
        ("http", include_str!("../docs/lang/http.fai")),
        ("filtering", include_str!("../docs/lang/filtering.fai")),
        ("rpc", include_str!("../docs/lang/rpc.fai")),
        ("signals", include_str!("../docs/lang/signals.fai")),
        ("storage", include_str!("../docs/lang/storage.fai")),
        ("env", include_str!("../docs/lang/env.fai")),
        ("events", include_str!("../docs/lang/events.fai")),
        ("limits", include_str!("../docs/lang/limits.fai")),
        ("debugging", include_str!("../docs/lang/debugging.fai")),
    ];

    let overview = include_str!("../docs/lang/overview.md");
    let mut entries = Vec::new();

    // Root `lang` entry — the overview text, shown when user runs `fai doc lang`.
    entries.push(DocEntry {
        namespace: String::new(), // root-level: shown in `fai doc` listing
        name: "lang".to_string(),
        full_path: "lang".to_string(),
        signature: String::new(),
        doc: overview.trim().to_string(),
        source: DocSource::Language,
        kind: EntryKind::LanguageTopic,
    });

    for (stem, content) in files {
        entries.extend(parse_lang_doc_file(content, stem));
    }

    entries
}

/// Parse `doc_*` functions from a lang doc .fai file into DocEntry items.
/// - Namespace:  `lang.<stem>`
/// - Name:       function name with `doc_` prefix stripped
/// - Doc:        leading `# ...` comment block
/// - Example:    function body rendered as a fenced code block
fn parse_lang_doc_file(source: &str, stem: &str) -> Vec<DocEntry> {
    let namespace = format!("lang.{}", stem);
    let mut entries = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Look for `def doc_<name>` lines.
        if let Some(rest) = trimmed.strip_prefix("def doc_") {
            let fn_name = rest.split_whitespace().next().unwrap_or("").to_string();
            if fn_name.is_empty() {
                i += 1;
                continue;
            }

            // Collect the leading doc comment lines above this def.
            let mut doc_lines: Vec<String> = Vec::new();
            let mut j = i;
            loop {
                if j == 0 {
                    break;
                }
                j -= 1;
                let prev = lines[j].trim();
                if prev.starts_with("# ") {
                    doc_lines.insert(0, prev[2..].to_string());
                } else if prev == "#" {
                    doc_lines.insert(0, String::new());
                } else {
                    break;
                }
            }

            // Collect the function body between `do` and the matching `end`.
            let mut body_lines: Vec<&str> = Vec::new();
            let mut depth: i32 = 0;
            let mut in_body = false;
            let mut k = i + 1;
            while k < lines.len() {
                let bl = lines[k].trim();
                if !in_body {
                    if bl.starts_with("@param")
                        || bl.starts_with("@return")
                        || bl.starts_with("@type")
                    {
                        k += 1;
                        continue;
                    }
                    if bl == "do" {
                        in_body = true;
                        k += 1;
                        continue;
                    }
                } else {
                    if bl == "end" && depth == 0 {
                        k += 1;
                        break;
                    }
                    // Track nested block depth.
                    if bl.starts_with("if ")
                        || bl.starts_with("while ")
                        || bl.starts_with("for ")
                        || bl == "do"
                        || bl.starts_with("do ")
                    {
                        depth += 1;
                    } else if bl == "end" {
                        depth -= 1;
                    }
                    body_lines.push(lines[k]);
                }
                k += 1;
            }

            // Trim leading/trailing blank lines from the body.
            while body_lines.first().map_or(false, |l| l.trim().is_empty()) {
                body_lines.remove(0);
            }
            while body_lines.last().map_or(false, |l| l.trim().is_empty()) {
                body_lines.pop();
            }

            let doc_text = doc_lines.join("\n");
            let example = if body_lines.is_empty() {
                String::new()
            } else {
                // Dedent by stripping common leading spaces.
                let min_indent = body_lines
                    .iter()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.len() - l.trim_start().len())
                    .min()
                    .unwrap_or(0);
                let dedented: Vec<String> = body_lines
                    .iter()
                    .map(|l| {
                        if l.len() >= min_indent {
                            l[min_indent..].to_string()
                        } else {
                            l.trim().to_string()
                        }
                    })
                    .collect();

                // When every non-empty line is a comment (i.e. `#` was
                // used on each line because the `doc_X` body had to
                // parse as valid fai but the example is really
                // conceptual / multi-file / pseudo-code), unprefix so
                // the reader sees clean code instead of a wall of
                // commented lines. Topics with mixed real fai code
                // (let, var, for) go through unchanged.
                let all_comments = dedented
                    .iter()
                    .filter(|l| !l.trim().is_empty())
                    .all(|l| l.trim_start().starts_with('#'));
                let final_lines: Vec<String> = if all_comments {
                    dedented
                        .iter()
                        .map(|l| {
                            let trimmed_left = l.trim_start();
                            let indent_len = l.len() - trimmed_left.len();
                            let indent = &l[..indent_len];
                            if let Some(rest) = trimmed_left.strip_prefix("# ") {
                                format!("{}{}", indent, rest)
                            } else if let Some(rest) = trimmed_left.strip_prefix('#') {
                                format!("{}{}", indent, rest)
                            } else {
                                l.clone()
                            }
                        })
                        .collect()
                } else {
                    dedented
                };
                format!("\n```fai\n{}\n```", final_lines.join("\n"))
            };

            let full_doc = if example.is_empty() {
                doc_text
            } else {
                format!("{}{}", doc_text, example)
            };

            entries.push(DocEntry {
                namespace: namespace.clone(),
                name: fn_name.clone(),
                full_path: format!("{}.{}", namespace, fn_name),
                // Use the topic name as the "signature" so the list view has
                // something to show in the left column.
                signature: fn_name.clone(),
                doc: full_doc,
                source: DocSource::Language,
                kind: EntryKind::LanguageTopic,
            });

            i = k;
            continue;
        }

        i += 1;
    }

    entries
}

// ── Package overview docs ─────────────────────────────────────────────────────

/// Collect overview documentation for a dependency package.
/// Reads the file(s) specified by the `docs` attribute in the package's fai.toml.
/// Returns a single entry with full_path = dep_name and the overview text as doc.
pub fn collect_package_overview(dep_path: &Path, dep_name: &str) -> Option<DocEntry> {
    let toml_path = dep_path.join("fai.toml");
    let content = std::fs::read_to_string(&toml_path).ok()?;

    // Find `docs = "..."` under [project] section.
    let mut in_project = false;
    let mut docs_glob: Option<String> = None;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_project = t == "[project]";
            continue;
        }
        if !in_project {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            if k.trim() == "docs" {
                docs_glob = Some(v.trim().trim_matches('"').to_string());
                break;
            }
        }
    }

    let pattern = docs_glob?;

    // Collect matching files — support simple patterns: "README.md", "docs.md",
    // or "docs/*.md" (files in a subdirectory).
    let mut text_parts: Vec<String> = Vec::new();

    if pattern.contains('*') {
        // Glob-style: read all .md files in the directory part.
        let dir_part = pattern.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
        let dir = dep_path.join(dir_part);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut paths: Vec<_> = entries
                .flatten()
                .filter(|e| e.path().extension().map_or(false, |x| x == "md"))
                .map(|e| e.path())
                .collect();
            paths.sort();
            for p in paths {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    text_parts.push(t);
                }
            }
        }
    } else {
        // Single file.
        if let Ok(t) = std::fs::read_to_string(dep_path.join(&pattern)) {
            text_parts.push(t);
        }
    }

    if text_parts.is_empty() {
        return None;
    }

    let combined = text_parts.join("\n\n").trim().to_string();

    Some(DocEntry {
        namespace: String::new(),
        name: dep_name.to_string(),
        full_path: dep_name.to_string(),
        signature: String::new(),
        doc: combined,
        source: DocSource::PackageOverview(dep_name.to_string()),
        kind: EntryKind::PackageOverview,
    })
}

// ── Search ────────────────────────────────────────────────────────────────────

/// Search documentation entries by query.
///
/// Matching rules (applied in order):
/// 1. Empty query → return all entries sorted by full_path
/// 2. Exact match on full_path → return that single entry
/// 3. Prefix/namespace match → return all entries under that namespace
/// 4. Case-insensitive substring match on name or full_path → sorted by relevance
pub fn search_docs<'a>(entries: &'a [DocEntry], query: &str) -> Vec<&'a DocEntry> {
    if query.is_empty() {
        let mut result: Vec<&DocEntry> = entries.iter().collect();
        result.sort_by(|a, b| a.full_path.cmp(&b.full_path));
        return result;
    }

    // 1. Exact full_path match
    if let Some(entry) = entries.iter().find(|e| e.full_path == query) {
        return vec![entry];
    }

    // 2. Prefix match: `std.array` matches `std.array.join`, etc.
    //    Also matches entries whose namespace equals query exactly.
    let prefix = format!("{}.", query);
    let mut prefix_matches: Vec<&DocEntry> = entries
        .iter()
        .filter(|e| e.full_path.starts_with(&prefix) || e.namespace == query)
        .collect();
    if !prefix_matches.is_empty() {
        prefix_matches.sort_by(|a, b| a.full_path.cmp(&b.full_path));
        return prefix_matches;
    }

    // 3. Substring match on name or full_path (case-insensitive)
    let q_lower = query.to_lowercase();
    let mut fuzzy: Vec<&DocEntry> = entries
        .iter()
        .filter(|e| {
            e.name.to_lowercase().contains(&q_lower)
                || e.full_path.to_lowercase().contains(&q_lower)
        })
        .collect();
    fuzzy.sort_by(|a, b| {
        let sa = match_score(&a.name, &q_lower);
        let sb = match_score(&b.name, &q_lower);
        sb.cmp(&sa).then(a.full_path.cmp(&b.full_path))
    });
    fuzzy
}

/// If `query` is an intermediate namespace (has sub-namespaces), return immediate child summaries.
/// Returns `None` if the query is a leaf namespace or doesn't match any namespace.
///
/// Examples:
/// - `"std"` → `Some([std.array, std.cli, ..., std.http, ...])`
/// - `"std.http"` → `Some([std.http.request, std.http.server])`
/// - `"std.http.server"` → `None` (leaf — use `search_docs` instead)
/// - `"Forui"` → `None` (leaf)
pub fn query_child_namespaces(entries: &[DocEntry], query: &str) -> Option<Vec<NamespaceSummary>> {
    // Empty query: list all top-level namespaces (root entries like `lang`, `std`,
    // package names, and a `<project>` bucket for local functions).
    if query.is_empty() {
        let mut roots: BTreeMap<String, usize> = BTreeMap::new();
        for e in entries {
            // Root entries (namespace == "") count as their own top-level item.
            if e.namespace.is_empty() {
                roots.entry(e.full_path.clone()).or_insert(0);
            } else {
                // Group by first path segment: "std.array.join" → "std"
                let root = e.full_path.split('.').next().unwrap_or(&e.full_path);
                *roots.entry(root.to_string()).or_insert(0) += 1;
            }
        }
        if roots.is_empty() {
            return None;
        }
        let summaries = roots
            .into_iter()
            .map(|(path, fn_count)| {
                let child_prefix = format!("{}.", path);
                let has_children = entries
                    .iter()
                    .any(|e| e.namespace.starts_with(&child_prefix));
                NamespaceSummary {
                    path,
                    fn_count,
                    has_children,
                }
            })
            .collect();
        return Some(summaries);
    }

    let prefix_dot = format!("{}.", query);

    // Fast check: are there any entries in sub-namespaces of query?
    if !entries.iter().any(|e| e.namespace.starts_with(&prefix_dot)) {
        return None;
    }

    // Aggregate function counts per immediate child namespace.
    let mut children: BTreeMap<String, usize> = BTreeMap::new();

    for e in entries {
        if e.namespace.starts_with(&prefix_dot) {
            // e.g. query="std", e.namespace="std.http.server"
            //   rest = "http.server", immediate = "http" → child = "std.http"
            let rest = &e.namespace[prefix_dot.len()..];
            let immediate = rest.split('.').next().unwrap_or(rest);
            let child_ns = format!("{}.{}", query, immediate);
            *children.entry(child_ns).or_insert(0) += 1;
        }
    }

    if children.is_empty() {
        return None;
    }

    let summaries = children
        .into_iter()
        .map(|(path, fn_count)| {
            let child_prefix = format!("{}.", path);
            let has_children = entries
                .iter()
                .any(|e| e.namespace.starts_with(&child_prefix));
            NamespaceSummary {
                path,
                fn_count,
                has_children,
            }
        })
        .collect();

    Some(summaries)
}

/// Render a namespace directory listing to stdout.
pub fn render_namespace_listing(summaries: &[NamespaceSummary]) {
    let use_color = std::io::stdout().is_terminal();
    let path_width = summaries.iter().map(|s| s.path.len()).max().unwrap_or(20);

    for s in summaries {
        let count_str = if s.fn_count == 1 {
            "1 function ".to_string()
        } else {
            format!("{} functions", s.fn_count)
        };
        // Append a hint when the namespace itself has sub-namespaces to explore.
        let hint = if s.has_children { "  …" } else { "" };

        if use_color {
            println!(
                "{:<path_width$}  \x1b[2m{}{}\x1b[0m",
                s.path,
                count_str,
                hint,
                path_width = path_width + 2
            );
        } else {
            println!(
                "{:<path_width$}  {}{}",
                s.path,
                count_str,
                hint,
                path_width = path_width + 2
            );
        }
    }
}

fn match_score(name: &str, query: &str) -> u8 {
    let n = name.to_lowercase();
    if n == query {
        3
    } else if n.starts_with(query) {
        2
    } else {
        1
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/// Print documentation results to stdout as markdown.
///
/// - Single result → full detail view (heading, def block, prose, example)
/// - Multiple results → grouped list view (types, enums, functions)
pub fn render_docs(entries: &[&DocEntry]) {
    // `use_color` is unused now — output is pure markdown so terminal and
    // MCP clients see the same text. Kept the signatures accepting a bool
    // only to limit churn; pass `false` from here.
    if entries.len() == 1 {
        render_detail(entries[0], false);
    } else {
        render_list(entries, false);
    }
}

/// Print search results. Multiple results include exact follow-up commands so
/// ambiguous name searches are easy to refine.
pub fn render_search_results(entries: &[&DocEntry]) {
    if entries.len() > 1 {
        println!("Multiple matches. Try:");
        for command in suggested_doc_commands(entries) {
            println!("- `{}`", command);
        }
        println!();
    }
    render_docs(entries);
}

fn suggested_doc_commands(entries: &[&DocEntry]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut commands = Vec::new();
    for entry in entries {
        if seen.insert(entry.full_path.as_str()) {
            commands.push(format!("fai doc {}", entry.full_path));
        }
    }
    commands
}

fn render_detail(entry: &DocEntry, _use_color: bool) {
    // Pure markdown — same output on terminal and via MCP. Agents reliably
    // parse markdown structure (headings, fenced code blocks, lists); the
    // prior ANSI + unicode divider was terminal-only eye-candy that forced
    // MCP clients to strip escapes.
    println!("# {}", entry.full_path);
    println!();

    let source_label = match &entry.source {
        DocSource::Stdlib => "stdlib".to_string(),
        DocSource::Project => "project".to_string(),
        DocSource::Dependency(name) => format!("{} (dependency)", name),
        DocSource::Language => "language reference".to_string(),
        DocSource::PackageOverview(name) => format!("{} (package)", name),
    };
    println!("__Source__: {}", source_label);
    if let Some(import) = import_line_for(entry) {
        println!("__Import__: `{}`", import);
    }
    println!();

    // Code block showing the forai-native form of the declaration. Agents
    // asked us to stop emitting C-style `fn(a: T) -> R` because it doesn't
    // match how they actually write fai code.
    match entry.kind {
        EntryKind::Function => {
            println!("```fai");
            println!(
                "{}",
                signature_to_def_block(&entry.signature).unwrap_or_else(|| entry.signature.clone())
            );
            println!("```");
            println!();
        }
        EntryKind::Type => {
            println!("```fai");
            println!("{}", entry.signature);
            println!("```");
            println!();
        }
        EntryKind::Enum => {
            println!("```fai");
            println!("{}", entry.signature);
            println!("```");
            println!();
        }
        // Language topics and package overviews: prose bodies already
        // contain their own code blocks. Don't wrap anything.
        EntryKind::LanguageTopic | EntryKind::PackageOverview => {}
    }

    let cleaned = clean_doc(&entry.doc);
    if matches!(entry.kind, EntryKind::PackageOverview) {
        if !cleaned.trim().is_empty() {
            println!("{}", cleaned.trim_end());
            println!();
        }
        return;
    }

    let (prose, example) = split_example(&cleaned);
    if !prose.trim().is_empty() {
        println!("{}", prose.trim_end());
        println!();
    }
    if let Some(ex) = example {
        println!("## Example");
        println!();
        println!("```fai");
        println!("{}", ex.trim());
        println!("```");
        println!();
    }
}

/// Convert a compact one-line function signature (`name(a: A, b: B) -> R`
/// or `name(a: A, b: B) -> (R1, R2)`) into a multi-line `def…end` block
/// matching how functions are actually written in fai. Returns None when
/// the input doesn't look like a function signature (caller should fall
/// back to printing the sig verbatim).
fn signature_to_def_block(sig: &str) -> Option<String> {
    let open = sig.find('(')?;
    // Closing paren of the parameter list — must pair with `open`. We walk
    // manually so nested parens in function-typed params (e.g.
    // `onClick: (ViewNode) -> Void`) don't end the scan early.
    let mut depth = 0i32;
    let mut close = None;
    for (i, ch) in sig.char_indices().skip(open) {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let name = sig[..open].trim();
    let params_str = &sig[open + 1..close];
    let after = sig[close + 1..].trim();
    // Return type is everything after `->`, if present.
    let returns = after.strip_prefix("->").map(|s| s.trim().to_string());

    let mut out = format!("def {}", name);
    for p in split_top_level_commas(params_str) {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        // Each param is `name: Type`. Rewrite as `@param name Type` (fai
        // form — no colon between name and type).
        if let Some((pname, pty)) = p.split_once(':') {
            out.push_str(&format!("\n    @param {} {}", pname.trim(), pty.trim()));
        } else {
            // No colon — give up and fall back to the raw sig.
            return None;
        }
    }
    if let Some(r) = returns {
        if r.is_empty() || r == "Void" {
            out.push_str("\n    @return Void");
        } else {
            out.push_str(&format!("\n    @return {}", r));
        }
    }
    out.push_str("\nend");
    Some(out)
}

/// Split a comma-separated list while respecting nested parentheses and
/// square brackets. `A, B, (C, D)` → `["A", "B", "(C, D)"]`.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if start <= s.len() {
        out.push(s[start..].to_string());
    }
    out
}

/// Recognise an "example:" block or a fenced ```fai ... ``` block in the
/// doc body and hoist it into its own `## Example` section. Returns
/// (prose_without_example, Option<example_body>).
fn split_example(doc: &str) -> (String, Option<String>) {
    // 1. Fenced code block — `\`\`\`fai ... \`\`\`` or `\`\`\` ... \`\`\``.
    if let Some(start) = doc.find("```") {
        // Find the end of the opening fence's info string (up to newline).
        let after_open = &doc[start + 3..];
        if let Some(nl) = after_open.find('\n') {
            let body_start = start + 3 + nl + 1;
            if let Some(end_rel) = doc[body_start..].find("```") {
                let body_end = body_start + end_rel;
                let prose = format!("{}{}", &doc[..start], &doc[body_end + 3..],);
                let example = doc[body_start..body_end].to_string();
                return (prose.trim().to_string(), Some(example));
            }
        }
    }

    // 2. Legacy `example:` marker — everything after the line is the example.
    for (i, line) in doc.lines().enumerate() {
        if line.trim().eq_ignore_ascii_case("example:") {
            let before: Vec<&str> = doc.lines().take(i).collect();
            let after: Vec<&str> = doc.lines().skip(i + 1).collect();
            let example = after.join("\n").trim().to_string();
            if !example.is_empty() {
                return (before.join("\n").trim().to_string(), Some(example));
            }
        }
    }

    (doc.to_string(), None)
}

/// Generate the forai import statement for a doc entry, or None if no import is needed
/// (project-local functions, language docs, package overviews).
fn import_line_for(entry: &DocEntry) -> Option<String> {
    // Language docs and overview pages are not imported.
    if matches!(entry.kind, EntryKind::PackageOverview) {
        return None;
    }
    match &entry.source {
        DocSource::Language | DocSource::PackageOverview(_) => return None,
        _ => {}
    }

    // Project-local functions — same module, no import needed.
    if entry.namespace.is_empty() {
        return None;
    }

    // Types and enums: use the type name directly from the namespace.
    // Functions and enums: `use { name } from namespace`
    Some(format!("use {{ {} }} from {}", entry.name, entry.namespace))
}

fn render_list(entries: &[&DocEntry], _use_color: bool) {
    // Group first by namespace, then within each namespace by kind
    // (Types → Enums → Functions). Language-topic namespaces get their
    // own compact section so `fai doc lang` still reads well without
    // colliding with the new grouped format.
    let mut grouped: BTreeMap<&str, Vec<&DocEntry>> = BTreeMap::new();
    for entry in entries {
        grouped
            .entry(entry.namespace.as_str())
            .or_default()
            .push(entry);
    }

    let mut first = true;
    for (namespace, group) in &grouped {
        if !first {
            println!();
        }
        first = false;

        if !namespace.is_empty() {
            println!("# {}", namespace);
            println!();
        }

        // Split by kind so we can group Types/Enums/Functions separately.
        let mut types: Vec<&DocEntry> = Vec::new();
        let mut enums: Vec<&DocEntry> = Vec::new();
        let mut functions: Vec<&DocEntry> = Vec::new();
        let mut topics: Vec<&DocEntry> = Vec::new();
        let mut overviews: Vec<&DocEntry> = Vec::new();
        for entry in group {
            match entry.kind {
                EntryKind::Type => types.push(entry),
                EntryKind::Enum => enums.push(entry),
                EntryKind::Function => functions.push(entry),
                EntryKind::LanguageTopic => topics.push(entry),
                EntryKind::PackageOverview => overviews.push(entry),
            }
        }

        if !types.is_empty() {
            println!("## Types");
            for e in &types {
                // Only the first line of the type signature — the full
                // `type Name\n...\nend` body belongs to the detail view.
                let head = e.signature.lines().next().unwrap_or(&e.name);
                // Strip the `type ` prefix to match the user's preferred
                // compact listing (`Name`, not `type Name`).
                let shown = head.strip_prefix("type ").unwrap_or(head);
                println!("- {}", shown);
            }
            println!();
        }
        if !enums.is_empty() {
            println!("## Enums");
            for e in &enums {
                // Enum signature is already a compact one-liner like
                // `enum SignalStatus { initial | loading | loaded }`.
                println!("- {}", e.signature);
            }
            println!();
        }
        if !functions.is_empty() {
            println!("## Functions");
            for e in &functions {
                // Prepend `def` so the listing reads as forai, not C —
                // e.g. `def fontSize(node: ViewNode, size: Int) -> ViewNode`.
                println!("- def {}", e.signature);
                if let Some(summary) = doc_summary(&e.doc) {
                    println!("  {}", summary);
                }
            }
            println!();
        }
        if !topics.is_empty() {
            // Language topics are usually 3–6 per namespace and each one
            // carries a small, self-contained example. Showing them
            // inline here means agents don't have to drill into every
            // sub-topic to see the code — `fai doc variables` should
            // answer "how do I declare a let/var" with actual snippets,
            // not just a summary line.
            for e in &topics {
                let cleaned = clean_doc(&e.doc);
                let (prose, example) = split_example(&cleaned);
                println!("## {}", e.name);
                println!();
                if !prose.trim().is_empty() {
                    println!("{}", prose.trim_end());
                    println!();
                }
                if let Some(ex) = example {
                    println!("```fai");
                    println!("{}", ex.trim_end());
                    println!("```");
                    println!();
                }
            }
        }
        if !overviews.is_empty() {
            for e in &overviews {
                let summary = clean_doc(&e.doc)
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string();
                if summary.is_empty() {
                    println!("- {}", e.name);
                } else {
                    println!("- {} — {}", e.name, summary);
                }
            }
            println!();
        }
    }
}

fn doc_summary(doc: &str) -> Option<String> {
    let cleaned = clean_doc(doc);
    let (prose, _) = split_example(&cleaned);
    let mut paragraph = Vec::new();

    for line in prose.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if paragraph.is_empty() {
                continue;
            }
            break;
        }
        if trimmed.starts_with('#') || trimmed.starts_with("```") {
            continue;
        }
        paragraph.push(trimmed);
    }

    if paragraph.is_empty() {
        None
    } else {
        Some(paragraph.join(" "))
    }
}

/// Strip leading section-separator noise from a doc comment, keeping only
/// the last meaningful paragraph (the lines closest to `def`).
fn clean_doc(doc: &str) -> String {
    // Walk lines in reverse, collecting the last contiguous block of
    // non-separator lines, then reverse back.
    let mut result: Vec<&str> = Vec::new();
    for line in doc.lines().rev() {
        let t = line.trim();
        if t.starts_with('─') {
            // Hit a section separator — stop collecting.
            break;
        }
        result.push(line);
    }
    // Drop leading/trailing empty lines from the collected block.
    while result.last().map_or(false, |l: &&str| l.trim().is_empty()) {
        result.pop();
    }
    if result.is_empty() {
        // Doc was nothing but separators — return empty so the caller shows nothing.
        return String::new();
    }
    result.reverse();
    while result.first().map_or(false, |l: &&str| l.trim().is_empty()) {
        result.remove(0);
    }
    result.join("\n")
}

// ── Signature rendering ───────────────────────────────────────────────────────

fn render_builtin_sig(name: &str, sig: &fai_checker::types::FunctionSig) -> String {
    let params: Vec<String> = sig
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, builtin_type_str(&p.ty)))
        .collect();

    let ret_str = if sig.returns.is_empty() {
        "Void".to_string()
    } else {
        sig.returns
            .iter()
            .map(builtin_type_str)
            .collect::<Vec<_>>()
            .join(", ")
    };

    format!("{}({}) -> {}", name, params.join(", "), ret_str)
}

fn builtin_type_str(ty: &fai_checker::types::Type) -> String {
    use fai_checker::types::Type;
    match ty {
        Type::Int => "Int".to_string(),
        Type::Float => "Float".to_string(),
        Type::String => "String".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::Dictionary => "Dictionary".to_string(),
        Type::Error => "Error".to_string(),
        Type::Void => "Void".to_string(),
        Type::Unknown => "Unknown".to_string(),
        Type::Null => "null".to_string(),
        Type::Never => "Never".to_string(),
        Type::Array(inner) => format!("{}[]", builtin_type_str(inner)),
        Type::Optional(inner) => format!("{}?", builtin_type_str(inner)),
        Type::Tuple(items) => {
            let parts: Vec<_> = items.iter().map(builtin_type_str).collect();
            format!("({})", parts.join(", "))
        }
        Type::Named { name, .. } => name.clone(),
        // No $ prefix — cleaner for display
        Type::TypeParameter(name) => name.clone(),
        Type::Function(sig) => {
            let params: Vec<_> = sig.params.iter().map(|p| builtin_type_str(&p.ty)).collect();
            let returns: Vec<_> = sig.returns.iter().map(builtin_type_str).collect();
            format!("({}) -> {}", params.join(", "), returns.join(", "))
        }
        _ => "Unknown".to_string(),
    }
}

fn render_parser_fn_sig(fd: &fai_parser::ast::FunctionDeclaration) -> String {
    let params: Vec<String> = fd
        .params
        .iter()
        .map(|p| format!("{}: {}", p.name, ast_type_str(&p.type_node)))
        .collect();

    let ret_str = match fd.return_types.len() {
        0 => "Void".to_string(),
        1 => ast_type_str(&fd.return_types[0].type_node),
        _ => fd
            .return_types
            .iter()
            .map(|r| {
                let ty = ast_type_str(&r.type_node);
                if let Some(ref label) = r.name {
                    format!("{}: {}", label, ty)
                } else {
                    ty
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    };

    format!("{}({}) -> {}", fd.name, params.join(", "), ret_str)
}

fn ast_type_str(tn: &fai_parser::ast::TypeNode) -> String {
    if let Some(ref params) = tn.function_params {
        let param_strs: Vec<String> = params.iter().map(ast_type_str).collect();
        let ret_strs: Vec<String> = tn
            .function_returns
            .as_ref()
            .map(|r| r.iter().map(ast_type_str).collect())
            .unwrap_or_default();
        let base = format!("({}) -> {}", param_strs.join(", "), ret_strs.join(", "));
        return wrap_type(base, tn.is_array, tn.is_optional);
    }
    let base = tn.name.as_deref().unwrap_or("Unknown").to_string();
    wrap_type(base, tn.is_array, tn.is_optional)
}

fn wrap_type(mut s: String, is_array: bool, is_optional: bool) -> String {
    if is_array {
        s.push_str("[]");
    }
    if is_optional {
        s.push('?');
    }
    s
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_stdlib_docs_nonempty() {
        let entries = collect_stdlib_docs();
        assert!(!entries.is_empty(), "stdlib docs should not be empty");
    }

    #[test]
    fn test_stdlib_has_std_array_join() {
        let entries = collect_stdlib_docs();
        let e = entries.iter().find(|e| e.full_path == "std.array.join");
        assert!(e.is_some(), "expected std.array.join in stdlib docs");
        let e = e.unwrap();
        assert!(
            e.signature.contains("join("),
            "signature should contain 'join('"
        );
        assert!(!e.doc.is_empty(), "doc should not be empty");
    }

    #[test]
    fn test_stdlib_has_std_http_server_ok() {
        let entries = collect_stdlib_docs();
        let e = entries.iter().find(|e| e.full_path == "std.http.server.ok");
        assert!(e.is_some(), "expected std.http.server.ok");
        let e = e.unwrap();
        assert!(
            e.signature.contains("ok("),
            "signature should start with ok("
        );
    }

    // ── signature_to_def_block ───────────────────────────────────────

    #[test]
    fn test_sig_to_def_block_simple() {
        let out = signature_to_def_block("add(a: Int, b: Int) -> Int").unwrap();
        assert_eq!(
            out,
            "def add\n    @param a Int\n    @param b Int\n    @return Int\nend"
        );
    }

    #[test]
    fn test_sig_to_def_block_no_params() {
        let out = signature_to_def_block("pi() -> Float").unwrap();
        assert_eq!(out, "def pi\n    @return Float\nend");
    }

    #[test]
    fn test_sig_to_def_block_void_return() {
        let out = signature_to_def_block("log(msg: String) -> Void").unwrap();
        assert!(out.contains("@param msg String"));
        assert!(out.contains("@return Void"));
    }

    #[test]
    fn test_sig_to_def_block_function_param_preserves_nesting() {
        // Callback-typed params must stay intact — `(ViewNode) -> Void`
        // used to be split on the first comma, producing garbage.
        let out = signature_to_def_block(
            "onClick(node: ViewNode, handler: (ViewNode) -> Void) -> ViewNode",
        )
        .unwrap();
        assert!(out.contains("@param node ViewNode"), "got: {}", out);
        assert!(
            out.contains("@param handler (ViewNode) -> Void"),
            "got: {}",
            out
        );
        assert!(out.contains("@return ViewNode"));
    }

    #[test]
    fn test_sig_to_def_block_rejects_non_function() {
        // Only function signatures (`name(...)`) should be converted. A
        // bare type name has no parens so we return None and let the
        // caller fall back to printing the signature as-is.
        assert!(signature_to_def_block("type Foo").is_none());
    }

    // ── import_line_for ───────────────────────────────────────────────

    #[test]
    fn test_import_line_stdlib_function() {
        let e = DocEntry {
            namespace: "std.convert".to_string(),
            name: "parseInt".to_string(),
            full_path: "std.convert.parseInt".to_string(),
            signature: "parseInt(text: String) -> Int".to_string(),
            doc: "Parse an integer.".to_string(),
            source: DocSource::Stdlib,
            kind: EntryKind::Function,
        };
        let import = import_line_for(&e);
        assert_eq!(
            import,
            Some("use { parseInt } from std.convert".to_string())
        );
    }

    #[test]
    fn test_import_line_dependency_function() {
        let e = DocEntry {
            namespace: "Forui.signal".to_string(),
            name: "isLoading".to_string(),
            full_path: "Forui.signal.isLoading".to_string(),
            signature: "isLoading(signal: Signal) -> Bool".to_string(),
            doc: "Check if loading.".to_string(),
            source: DocSource::Dependency("Forui".to_string()),
            kind: EntryKind::Function,
        };
        let import = import_line_for(&e);
        assert_eq!(
            import,
            Some("use { isLoading } from Forui.signal".to_string())
        );
    }

    #[test]
    fn test_import_line_project_local_no_import() {
        let e = DocEntry {
            namespace: String::new(),
            name: "myFn".to_string(),
            full_path: "myFn".to_string(),
            signature: "myFn() -> Void".to_string(),
            doc: String::new(),
            source: DocSource::Project,
            kind: EntryKind::Function,
        };
        assert_eq!(import_line_for(&e), None, "project-local needs no import");
    }

    #[test]
    fn test_import_line_language_doc_no_import() {
        let e = DocEntry {
            namespace: "lang.variables".to_string(),
            name: "let".to_string(),
            full_path: "lang.variables.let".to_string(),
            signature: "let".to_string(),
            doc: "Immutable binding.".to_string(),
            source: DocSource::Language,
            kind: EntryKind::LanguageTopic,
        };
        assert_eq!(import_line_for(&e), None, "language docs need no import");
    }

    #[test]
    fn test_import_line_package_overview_no_import() {
        let e = DocEntry {
            namespace: String::new(),
            name: "Forui".to_string(),
            full_path: "Forui".to_string(),
            signature: String::new(),
            doc: "Forui overview.".to_string(),
            source: DocSource::PackageOverview("Forui".to_string()),
            kind: EntryKind::PackageOverview,
        };
        assert_eq!(
            import_line_for(&e),
            None,
            "package overview needs no import"
        );
    }

    #[test]
    fn test_search_empty_query_returns_all_sorted() {
        let entries = collect_stdlib_docs();
        let results = search_docs(&entries, "");
        assert_eq!(results.len(), entries.len());
        for w in results.windows(2) {
            assert!(w[0].full_path <= w[1].full_path);
        }
    }

    #[test]
    fn test_search_exact_match() {
        let entries = collect_stdlib_docs();
        let results = search_docs(&entries, "std.array.join");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].full_path, "std.array.join");
    }

    #[test]
    fn test_search_prefix_match() {
        let entries = vec![
            DocEntry {
                namespace: "std.demo".to_string(),
                name: "first".to_string(),
                full_path: "std.demo.first".to_string(),
                signature: "first() -> Void".to_string(),
                doc: String::new(),
                source: DocSource::Stdlib,
                kind: EntryKind::Function,
            },
            DocEntry {
                namespace: "std.demo".to_string(),
                name: "second".to_string(),
                full_path: "std.demo.second".to_string(),
                signature: "second() -> Void".to_string(),
                doc: String::new(),
                source: DocSource::Stdlib,
                kind: EntryKind::Function,
            },
        ];
        let results = search_docs(&entries, "std.demo");
        assert!(
            results.len() > 1,
            "prefix match should return multiple results for intermediate namespaces"
        );
        assert!(results.iter().all(|e| e.full_path.starts_with("std.demo.")));
    }

    #[test]
    fn test_stdlib_module_overview_entries_exist() {
        let entries = collect_stdlib_docs();
        let array = entries
            .iter()
            .find(|e| e.full_path == "std.array" && matches!(e.kind, EntryKind::PackageOverview));
        assert!(array.is_some(), "std.array overview should exist");
        assert_eq!(
            import_line_for(array.unwrap()),
            None,
            "stdlib module overviews should not show function-style imports"
        );
        assert!(
            array.unwrap().doc.contains("array.map"),
            "std.array overview should include examples"
        );

        let json = entries
            .iter()
            .find(|e| e.full_path == "std.json" && matches!(e.kind, EntryKind::PackageOverview));
        assert!(json.is_some(), "std.json overview should exist");
        assert!(
            json.unwrap().doc.contains("json.parse"),
            "std.json overview should explain parsing"
        );
    }

    #[test]
    fn test_search_fuzzy_match() {
        let entries = collect_stdlib_docs();
        let results = search_docs(&entries, "join");
        assert!(!results.is_empty(), "fuzzy 'join' should match something");
        assert!(results.iter().any(|e| e.name == "join"));
    }

    #[test]
    fn test_search_no_match_returns_empty() {
        let entries = collect_stdlib_docs();
        let results = search_docs(&entries, "zzznomatch");
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_namespace_match() {
        let entries = vec![DocEntry {
            namespace: "std.demo".to_string(),
            name: "first".to_string(),
            full_path: "std.demo.first".to_string(),
            signature: "first() -> Void".to_string(),
            doc: String::new(),
            source: DocSource::Stdlib,
            kind: EntryKind::Function,
        }];
        let results = search_docs(&entries, "std.demo");
        assert!(!results.is_empty());
        assert!(results.iter().all(|e| e.namespace == "std.demo"));
    }

    #[test]
    fn test_suggested_doc_commands_use_exact_paths() {
        let entries = vec![
            DocEntry {
                namespace: "Forui.view".to_string(),
                name: "Button".to_string(),
                full_path: "Forui.view.Button".to_string(),
                signature: "Button(text: String) -> ViewNode".to_string(),
                doc: String::new(),
                source: DocSource::Dependency("Forui".to_string()),
                kind: EntryKind::Function,
            },
            DocEntry {
                namespace: "HtmlForui.html".to_string(),
                name: "renderButton".to_string(),
                full_path: "HtmlForui.html.renderButton".to_string(),
                signature: "renderButton() -> String".to_string(),
                doc: String::new(),
                source: DocSource::Dependency("HtmlForui".to_string()),
                kind: EntryKind::Function,
            },
        ];
        let refs: Vec<&DocEntry> = entries.iter().collect();
        assert_eq!(
            suggested_doc_commands(&refs),
            vec![
                "fai doc Forui.view.Button".to_string(),
                "fai doc HtmlForui.html.renderButton".to_string()
            ]
        );
    }

    #[test]
    fn test_doc_summary_uses_first_prose_paragraph() {
        let doc =
            "Build a button view.\nUse this for primary actions.\n\n```fai\nButton('Save')\n```";
        assert_eq!(
            doc_summary(doc),
            Some("Build a button view. Use this for primary actions.".to_string())
        );
    }

    #[test]
    fn test_stdlib_doc_source_is_stdlib() {
        let entries = collect_stdlib_docs();
        assert!(entries
            .iter()
            .all(|e| matches!(e.source, DocSource::Stdlib)));
    }

    #[test]
    fn test_query_child_namespaces_std() {
        let entries = collect_stdlib_docs();
        let children = query_child_namespaces(&entries, "std").expect("std should have children");
        let paths: Vec<&str> = children.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"std.array"), "expected std.array");
        assert!(
            paths.contains(&"std.http"),
            "expected std.http (intermediate)"
        );
        assert!(paths.contains(&"std.math"), "expected std.math");
        // std.http is intermediate — has sub-namespaces itself
        let http = children.iter().find(|s| s.path == "std.http").unwrap();
        assert!(http.has_children, "std.http should report has_children");
        // std.array is a leaf
        let arr = children.iter().find(|s| s.path == "std.array").unwrap();
        assert!(!arr.has_children, "std.array should not have children");
    }

    #[test]
    fn test_query_child_namespaces_std_http() {
        let entries = collect_stdlib_docs();
        let children =
            query_child_namespaces(&entries, "std.http").expect("std.http should have children");
        let paths: Vec<&str> = children.iter().map(|s| s.path.as_str()).collect();
        assert!(
            paths.contains(&"std.http.request"),
            "expected std.http.request"
        );
        assert!(
            paths.contains(&"std.http.server"),
            "expected std.http.server"
        );
        assert_eq!(paths.len(), 2, "std.http should have exactly 2 children");
    }

    #[test]
    fn test_query_child_namespaces_leaf_returns_none() {
        let entries = collect_stdlib_docs();
        // std.array is a leaf namespace
        assert!(query_child_namespaces(&entries, "std.array").is_none());
        // std.http.server is a leaf namespace
        assert!(query_child_namespaces(&entries, "std.http.server").is_none());
    }

    #[test]
    fn test_query_child_namespaces_empty_returns_root_namespaces() {
        // Empty query now returns top-level namespaces (lang, std, etc.)
        let entries = collect_stdlib_docs();
        let roots = query_child_namespaces(&entries, "");
        assert!(roots.is_some(), "empty query should return root namespaces");
        let roots = roots.unwrap();
        assert!(
            roots.iter().any(|r| r.path == "std"),
            "should include 'std'"
        );
    }

    #[test]
    fn test_query_child_namespaces_fn_counts() {
        let entries = collect_stdlib_docs();
        let children = query_child_namespaces(&entries, "std.http").unwrap();
        let server = children
            .iter()
            .find(|s| s.path == "std.http.server")
            .unwrap();
        let request = children
            .iter()
            .find(|s| s.path == "std.http.request")
            .unwrap();
        assert_eq!(server.fn_count, 10);
        assert_eq!(request.fn_count, 5);
    }

    // ── Lang doc tests ────────────────────────────────────────────────

    #[test]
    fn test_lang_docs_nonempty() {
        let entries = collect_lang_docs();
        assert!(!entries.is_empty(), "lang docs should not be empty");
    }

    #[test]
    fn test_lang_root_entry_exists() {
        let entries = collect_lang_docs();
        let root = entries.iter().find(|e| e.full_path == "lang");
        assert!(
            root.is_some(),
            "should have a root 'lang' entry for the overview"
        );
        assert!(
            root.unwrap().doc.contains("forai"),
            "overview should mention forai"
        );
    }

    #[test]
    fn test_lang_variables_let_entry() {
        let entries = collect_lang_docs();
        let e = entries.iter().find(|e| e.full_path == "lang.variables.let");
        assert!(e.is_some(), "lang.variables.let should exist");
        let e = e.unwrap();
        assert!(e.doc.contains("immutable"), "doc should mention immutable");
        assert!(
            e.doc.contains("let x = 42"),
            "doc should contain example code"
        );
    }

    #[test]
    fn test_lang_modules_rpc_server_entry() {
        let entries = collect_lang_docs();
        let e = entries
            .iter()
            .find(|e| e.full_path == "lang.modules.rpc_server");
        assert!(e.is_some(), "lang.modules.rpc_server should exist");
        let e = e.unwrap();
        assert!(e.doc.contains("remote def"), "should explain remote def");
        assert!(
            e.doc.contains("generated RPC route installer"),
            "should mention generated RPC route installation"
        );
    }

    #[test]
    fn test_lang_searchable_by_name() {
        let entries = collect_lang_docs();
        // 'rpc_server' should be findable via substring search
        let results = search_docs(&entries, "rpc_server");
        assert!(!results.is_empty(), "rpc_server should be findable");
        assert!(results.iter().any(|e| e.full_path.contains("rpc_server")));
    }

    /// Part 1.7: `lang.limits` is registered and its page mentions
    /// every countable resource from `fai_core::limits::ALL_LIMITS`
    /// (except the internal `wasm native method ids` which we document
    /// inline but don't need to spell out for end users). If someone
    /// adds a new limit to the registry without updating the doc, this
    /// test fails so they remember.
    #[test]
    fn test_lang_limits_doc_covers_every_registry_limit() {
        use fai_core::limits::ALL_LIMITS;

        let entries = collect_lang_docs();
        // Gather every lang.limits.* doc body into one blob so the
        // coverage check can inspect all sub-entries together.
        let blob: String = entries
            .iter()
            .filter(|e| e.full_path.starts_with("lang.limits"))
            .map(|e| e.doc.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!blob.is_empty(), "lang.limits.* entries must be collected",);
        for limit in ALL_LIMITS {
            assert!(
                blob.contains(limit.name),
                "lang.limits doc missing coverage for `{}` — add a section to docs/lang/limits.fai",
                limit.name,
            );
        }
    }

    #[test]
    fn test_lang_child_namespaces_under_lang() {
        let entries = collect_lang_docs();
        let children = query_child_namespaces(&entries, "lang");
        assert!(children.is_some(), "lang should have child namespaces");
        let children = children.unwrap();
        assert!(children.iter().any(|c| c.path == "lang.variables"));
        assert!(children.iter().any(|c| c.path == "lang.modules"));
        assert!(children.iter().any(|c| c.path == "lang.functions"));
        assert!(children.iter().any(|c| c.path == "lang.env"));
        assert!(children.iter().any(|c| c.path == "lang.events"));
    }

    // ── Type and recursive scan tests ─────────────────────────────────

    /// Create a uniquely-named temp directory for this test, clean it up on drop.
    fn make_tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fai_doc_test_{}", tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_collect_docs_from_dir_includes_type_declarations() {
        let dir = make_tmp("type_decl");
        std::fs::write(
            dir.join("types.fai"),
            "type Counter\n  count Int\n  label String\nend\n\n# Inc.\ndef increment\n    @param c Counter\n    @return Counter\ndo\n  c\nend",
        ).unwrap();

        let entries = collect_docs_from_dir(&dir, "mymod", DocSource::Project);

        let type_entry = entries.iter().find(|e| e.name == "Counter");
        assert!(type_entry.is_some(), "type Counter should be indexed");
        let te = type_entry.unwrap();
        assert_eq!(te.full_path, "mymod.Counter");
        assert!(
            te.signature.contains("count Int"),
            "signature should show fields"
        );
        assert!(te.signature.contains("label String"));

        let fn_entry = entries.iter().find(|e| e.name == "increment");
        assert!(fn_entry.is_some(), "function increment should be indexed");
    }

    #[test]
    fn test_collect_docs_recursive_scans_subdirectories() {
        let dir = make_tmp("recursive");
        std::fs::write(
            dir.join("main.fai"),
            "# Top fn.\ndef topFn\n    @return Void\ndo\nend",
        )
        .unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(
            dir.join("sub").join("helper.fai"),
            "# Sub fn.\ndef subFn\n    @return Void\ndo\nend",
        )
        .unwrap();

        let entries = collect_docs_recursive(&dir, "pkg", DocSource::Project);

        let top = entries.iter().find(|e| e.name == "topFn");
        assert!(
            top.is_some(),
            "topFn should be found in top-level directory"
        );
        assert_eq!(top.unwrap().namespace, "pkg");

        let sub = entries.iter().find(|e| e.name == "subFn");
        assert!(sub.is_some(), "subFn should be found in sub-directory");
        assert_eq!(
            sub.unwrap().namespace,
            "pkg.sub",
            "sub-directory functions get pkg.sub namespace"
        );
        assert_eq!(sub.unwrap().full_path, "pkg.sub.subFn");
    }

    #[test]
    fn test_collect_dependency_module_overviews_scans_subdirectories() {
        let dir = make_tmp("dep_module_overview");
        std::fs::create_dir_all(dir.join("src/view")).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"Pkg\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/view/docs.md"), "# View\n\nOverview text.").unwrap();

        let entries = collect_dependency_module_overviews(&dir, "Pkg");
        let overview = entries.iter().find(|e| e.full_path == "Pkg.view");
        assert!(overview.is_some(), "Pkg.view docs.md should be indexed");
        let overview = overview.unwrap();
        assert_eq!(overview.namespace, "Pkg");
        assert_eq!(overview.name, "view");
        assert!(matches!(overview.kind, EntryKind::PackageOverview));
        assert!(overview.doc.contains("Overview text"));
    }

    #[test]
    fn test_collect_dependency_docs_hides_private_declarations() {
        let dir = make_tmp("dep_private_docs");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"Pkg\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/api.fai"),
            "# Public API.\ndef publicApi\n    @return Void\ndo\nend\n\nprivate:\n\n# Private helper.\ndef privateHelper\n    @return Void\ndo\nend\n\ntype Secret\n    value String\nend\n\nenum HiddenState\n    ready\nend\n",
        )
        .unwrap();

        let entries = collect_dependency_docs(&dir, "Pkg");
        assert!(
            entries.iter().any(|e| e.full_path == "Pkg.publicApi"),
            "public dependency declarations should be indexed"
        );
        assert!(
            !entries.iter().any(|e| e.full_path == "Pkg.privateHelper"),
            "private dependency functions should be hidden"
        );
        assert!(
            !entries.iter().any(|e| e.full_path == "Pkg.Secret"),
            "private dependency types should be hidden"
        );
        assert!(
            !entries.iter().any(|e| e.full_path == "Pkg.HiddenState"),
            "private dependency enums should be hidden"
        );
    }

    #[test]
    fn test_enum_entries_indexed() {
        let dir = make_tmp("enum_decl");
        std::fs::write(
            dir.join("enums.fai"),
            "enum Status\n  active\n  loading\n  error\nend",
        )
        .unwrap();

        let entries = collect_docs_from_dir(&dir, "ns", DocSource::Project);
        let e = entries.iter().find(|e| e.name == "Status");
        assert!(e.is_some(), "enum Status should be indexed");
        assert!(e.unwrap().signature.contains("active | loading | error"));
    }
}
