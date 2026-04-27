# forai Language Reference

The primary user interface is the `fai` / `forai` CLI. Source files are
formatted, checked, tested, compiled directly to WebAssembly, and executed by
the CLI's WASM host runner.

Common commands:

```bash
fai fmt file.fai
fai check file.fai
fai test file.fai
fai run file.fai
fai build [target]
fai doc [query]
```

Pipeline commands run prerequisites in order: `fmt -> check -> test -> run/build`.

## Variables

```
let x = 42          # immutable — no reassignment, no field mutation
var count = 0       # mutable — can reassign, can mutate fields
count = count + 1   # reassignment (var only)

let name String = 'Alice'   # explicit type annotation
let age Int? = null          # optional type
```

### Mutability

`let` bindings are fully frozen — no reassignment, no field mutation:

```
let user = User(name: 'Alice', age: 30)
user.age = 31       # ERROR — cannot mutate field on let binding
user = otherUser    # ERROR — cannot reassign let binding
```

`var` bindings are fully mutable — reassignment and field/index mutation:

```
var user = User(name: 'Alice', age: 30)
user.age = 31       # OK — var allows field mutation
user.name = 'Bob'   # OK — all fields mutable at any depth

var items = [1 2 3]
items[0] = 99       # OK — var allows index mutation
```

### Value semantics

All assignments are deep copies. Every variable owns its value independently:

```
var a = User(name: 'Alice', age: 30)
var b = a           # deep copy — b is independent
b.age = 99
print(a.age)        # 30 — a is unchanged
print(b.age)        # 99

let frozen = a      # deep copy — frozen is an immutable snapshot
```

Nested objects are also deep copied:

```
var outer = Outer(inner: Inner(value: 1))
var copy = outer
copy.inner.value = 99
print(outer.inner.value)  # 1 — fully independent
```

### Function parameters

Function parameters are immutable by default (like `let`). The function receives its own copy unless the parameter is marked `mutable`:

```
# Cannot mutate params — return a new value instead.
def birthday
    @param user User
    @return User
do
    User(name: user.name, age: user.age + 1)
end
```

Use `mutable` when the function should receive the caller's mutable binding and be allowed to mutate it in place:

```
type Counter
  value Int
end

def increment
    @param counter Counter, mutable
    @return Void
do
    counter.value = counter.value + 1
end

var c = Counter(value: 0)
increment(c)
print(c.value)   # 1
```

Only `var` bindings can be passed to `mutable` parameters:

```
let frozen = Counter(value: 0)
increment(frozen)    # ERROR — cannot pass immutable binding to mutable param
```

## Types

### Primitive types

- `Int` — 32-bit integer
- `Float` — 64-bit float
- `String` — UTF-8 string
- `Bool` — `true` or `false`
- `Void` — no value

### Numeric literal annotations

Type annotations on numeric bindings follow these rules:

```
let x = 1            # OK — type inferred Int
let x Int = 1        # OK — Int annotation, Int literal
let x Int = 1.0      # OK — whole-valued Float narrows to Int
let x Int = 1.23     # ERROR — 1.23 is not an Int

let f = 1.0          # OK — type inferred Float
let f Float = 1.0    # OK — Float annotation, Float literal
let f Float = 1      # OK — Int widens to Float

let sci = 1.2e-3     # OK — scientific notation
```

Int widens to Float unconditionally (ints are a subset of floats).
Float narrows to Int only when the right-hand side is a literal whose
value is a whole number; `1.0` is equivalent to `1`, but `1.23` is
rejected. A Float *variable* or function-return value never silently
narrows — use an explicit `toInt(x)` cast.

### Optional types

```
let x Int? = null    # nullable
let y Int? = 42      # has a value
let z = y?           # optional check (returns Bool)
let w = y!           # force unwrap (panics if null)
```

### Arrays

```
let items = [1 2 3]         # array literal (space-separated)
let vertical = [
  1
  2
  3
]                           # vertical style is also valid
let first = items[0]        # index access
let count = length(items)   # length

var mutable = [1 2 3]
mutable[0] = 99             # index mutation (var only)
```

### Dictionaries

```
let d = { name: 'Alice' age: 30 }    # dict literal
let name = getString(d, 'name')       # access by key
let d2 = set(d, 'email', 'a@b.com')   # returns the updated dict value
let keys = getKeys(d)                 # get all keys
let has = hasKey(d, 'name')           # check key exists
```

### Tuples

```
let a, b = swap(1, 2)    # destructure multiple return values
```

### Ranges

```
for i in 0..10     # 0..9  — `..` is exclusive of the upper bound
for i in 0...10    # 0..10 — `...` is inclusive
```

Exclusive `..` is the common case (it matches array indexing:
`for i in 0..length(arr)` visits every valid index). Inclusive `...`
is handy when you want "count up to and including N".

## Functions

All named functions use the contract syntax with `@param`, `@return`, and `do...end`:

```
# Add two numbers.
def add
    @param a Int
    @param b Int
    @return Int
do
    a + b
end
```

### Structure

```
# Doc comment for the function (required for named functions, except main).
def <name>
    @type <TypeParam>           # generic type parameters (optional)
    @param <name> <Type>        # parameters
    @return [<name>] <Type>     # return value(s)
do
    <body>
end
```

- `@type`, `@param`, `@return` must appear in that order
- `# comments` above any annotation are captured as documentation
- Doc comments on individual `@param`/`@return` lines are optional

### Default values

```
# Connect to database.
def connect
    @param host String, default: 'localhost'
    @param port Int, default: 5432
    @return Connection
do
    ...
end
```

### Multiple return values

```
# Swap two values.
def swap
    @param a Int
    @param b Int
    @return first Int
    @return second Int
do
    b, a
end

let x, y = swap(1, 2)
```

### Generic functions

```
# Return value unchanged.
def echo
    @type T
    @param value T
    @return T
do
    value
end

let x = echo(42)       # T inferred as Int
let s = echo('hello')  # T inferred as String
```

### Calling conventions

```
add(1, 2)                                    # positional
createUser(name: 'Alice', email: 'a@b.com')  # named
createUser('Alice', email: 'a@b.com')        # mixed
```

### UFCS (Uniform Function Call Syntax)

Any function can be called as a method on its first parameter:

```
# Double a number.
def double
    @param n Int
    @return Int
do
    n * 2
end

let x = 5.double()          # same as double(5)
let y = 3.double().double()  # chaining
```

### Anonymous blocks (`do...end`)

```
let result = apply(do with n Int
    n * 3
end)
```

`do...end` blocks are anonymous closures. They do not require `@param`/`@return` annotations. The parameter types and return type are inferred from the `type def` they're matched against.

### Trailing `do...end`

When a function's last parameter is a `type def`, you can pass a `do...end` block after the call:

```
# No parens — block is the only argument:
run do
    print('hello')
end

# After parens — block appended as last argument:
apply(5) do with n Int
    n * 3
end

# Nested:
container do
    print('outer')
    container do
        print('inner')
    end
end
```

### main

`main` is the entry point. It is exempt from the doc comment requirement.

```
def main
    @return Void
do
    print('hello world')
end
```

## Function Types (`type def`)

Named function types define the signature for callbacks, event handlers, and closures:

```
type def ClickHandler
    @return Void
end

type def Transform
    @param n Int
    @return Int
end

type def Reducer
    @param state State
    @param action Action
    @return State
end
```

### Using function types

As parameters:

```
# Apply a transform.
def apply
    @param cb Transform
    @param value Int
    @return Int
do
    cb(value)
end

apply(do with n Int
    n * 2
end)
```

As type fields (including optional):

```
type Events
    onClick ClickHandler?
    onChange ChangeHandler?
end

# Check and call:
if events.onClick?
    events.onClick!()
end
```

As variable type annotations:

```
let doubler Transform = do with n Int
    n * 2
end
```

## Type Declarations

```
type Point
    x Int
    y Int
end

let p = Point(x: 1, y: 2)
print(p.x)
```

### Field mutation

```
var p = Point(x: 1, y: 2)
p.x = 10    # OK — var binding allows field mutation
```

### Generic types

```
type Box
    @type T
    value T
end

let b = Box(value: 42)   # T inferred as Int
```

## Enums

```
enum Color
    red
    green
    blue
end

let c = Color.red
```

### Case matching

```
case c
when Color.red
    print('red')
when Color.green
    print('green')
default
    print('other')
end
```

## Control Flow

### If / else

```
if x > 5
    print('big')
else if x > 0
    print('small')
else
    print('zero or negative')
end
```

### While

```
var i = 0
while i < 10
    print(i)
    i = i + 1
end
```

### For loops

```
for item in items
    print(item)
end

for i in 0..10
    print(i)
end
```

### Break and continue

```
for i in 0..100
    if i == 5
        break
    end
    if i == 3
        continue
    end
    print(i)
end
```

## Error Handling

```
use std.error

try
    throw Error('something went wrong')
catch e
    print(e.message)
    print(message(e))
end
```

`finally` runs unconditionally after the try/catch, whether or not an error was thrown:

```
try
    throw Error('oops')
catch e
    print(e.message)
finally
    print('always runs')
end
```

- `throw` works across function boundaries — the call stack unwinds to the nearest `catch`
- `Error('message')` or `error.Error('message')` constructs an error object
- `message(err)` / `error.message(err)` returns the message string
- `kind(err)` / `error.kind(err)` returns the error kind when present, otherwise `null`
- `isError(value)` / `error.isError(value)` checks if a value is an error
- `unwrap(value, fallback)` / `error.unwrap(value, fallback)` returns `value` unless it is `null`

## Operators

### Arithmetic
- `+` `-` `*` — add, subtract, multiply
- `/` — division (always returns Float)
- `//` — floor division (returns Int)
- `**` — power
- `%` — modulo

### Comparison
- `==` `!=` `>` `<` `>=` `<=`

### Logical
- `and` `or` — short-circuiting binary operators (RHS is skipped when LHS fixes the result)
- `not` — unary negation (keyword form)
- `!` — unary negation (symbol form; identical semantics to `not`)

Precedence: `not`/`!` bind tighter than `and`, which binds tighter than
`or`. So `not a and b or c` parses as `((not a) and b) or c`.

The C-style spellings `&&` and `||` are **not** part of the language —
the lexer rejects them.

### String
- `+` — concatenation
- `"hello {{name}}"` — template strings (double quotes, double-brace interpolation)
- `'literal'` — plain strings (single quotes)
- Escapes such as `\n`, `\t`, `\\`, `\'`, and `\"` are preserved through parsing and formatting

## Concurrency

### `nowait` — fire and forget

`nowait` spawns a task in the background without waiting for its result:

```
nowait sendEmail(user.email)
nowait logEvent('page_viewed')
```

### `all` — parallel tasks

`all` runs multiple closures concurrently and returns their results as a tuple. The caller blocks until all tasks complete:

```
let a, b = all(fetchUser(), fetchPosts())

# Single task:
let results = all(compute())

# Three tasks:
let x, y, z = all(taskA(), taskB(), taskC())
```

### `sleep` — delay

```
sleep(500)   # pause for 500 milliseconds
```

## Modules and Imports

A module is a **directory** of `.fai` files. Every public declaration in every `.fai` file in that directory is exported under the directory's name. **You import from the directory name, not the file name.**

### 1 — Same project, sibling directory

```
src/
  client/
    app.fai          # exports: App
    components/
      nav.fai        # exports: Nav
      section.fai    # exports: Section
    pages/
      home.fai       # exports: HomePage
      tasks.fai      # exports: TasksPage
    state/
      auth.fai       # exports: isLoggedIn, setToken
```

```fai
# From any file inside src/client/:
use { App } from client                    # all files in client/ → client module
use { Nav, Section } from client.components
use { HomePage, TasksPage } from client.pages
use { isLoggedIn } from client.state

# WRONG — you cannot import a single file
use { App } from client.app               # ERROR: looks for client/app/ directory
```

### 2 — Same project, cross-target (server imports client for SSR)

```fai
# src/server/main.fai — server importing the client UI for server-side rendering
use { App } from client                   # resolves to src/client/ (same source_root = "src")
```

When both `[project.client]` and `[project.server]` share `source = "src"` in fai.toml,
all directories under `src/` are visible to both targets by name.

### 3 — External package (declared in fai.toml `[dependencies]`)

fai.toml:
```toml
[dependencies]
"file:///home/user/mylibs/forui" = "0.1.0"
```

The package name comes from the dependency's own `fai.toml` `name` field. Import using
that name (capitalized by convention for packages):

```fai
use { mount } from Forui                  # top-level exports from forui/src/forui.fai
use { Label, Button, VStack } from Forui.view    # sub-module forui/src/view/
use { useSignal, isLoading } from Forui.signal   # sub-module forui/src/signal/
use { navigate, Link } from Forui.router         # sub-module forui/src/router/
```

**Every function used via UFCS (e.g. `node.fontSize(14)`) must be explicitly imported.**
The import is per-file — there is no global namespace. If the checker reports
`'fontSize' is not in scope`, add it to the `use { ... } from Forui.view` line in that file.

Common Forui imports for client pages:

```fai
use { ViewNode, VStack, HStack, Label, Button, TextInput,
      fontSize, fontWeight, foreground, padding, background,
      cornerRadius, opacity } from Forui.view
use { useSignal, isLoading, isLoaded, isError, reload, setValue } from Forui.signal
use { navigate, routeParam, Link, Router, Route } from Forui.router
```

### 4 — Fullstack RPC across sub-projects

In a multi-target fullstack project, `remote def` marks a function as callable
across the network. Client code imports remote functions from their real module
path; the build replaces those calls with generated RPC stubs for client targets.

```fai
# src/pages/tasks.fai
use { Task, getTasks, addTask } from data.tasks
```

Server targets expose remote functions through `addRpcRoutes`. Any `remote def`
reachable in the server target's build graph is available to `addRpcRoutes`.
Importing the module from the server entry is the usual way to include it in the
server RPC surface:

```fai
# src/platforms/server/main.fai
use std.http.server
use { getTasks, addTask } from data.tasks

def main
    @return Void
do
  var r = server.router()
  addRpcRoutes(r)       # generated from reachable remote defs
  server.listen(r, 3040)
end
```

The remote declarations live with the domain code:

```fai
# src/data/tasks/main.fai
remote type Task
  id Int
  text String
end

remote def getTasks
    @param token String
    @return Task[]
do
  ...
end

remote def addTask
    @param token String
    @param text String
    @return Task
do
  ...
end
```

A `remote def` that exists on disk but is not reachable from the server target's
imports is not exposed by that server. Non-remote helpers in the same module are
not part of the RPC API.

### Visibility

Declarations are public by default. Use `private:` to mark subsequent declarations as
module-private (unexported):

```fai
def publicFn           # exported
    @return Void
do end

private:
def helper             # NOT exported — everything after private: is private
    @return Void
do end
```

### Standard library

Use `fai doc std` to browse all standard library modules, or `fai doc std.array` /
`fai doc std.string` etc. for per-module docs. The full function signatures and
descriptions are available via `fai doc <name>` at any time.

Available modules: `std.string`, `std.array`, `std.dictionary`, `std.math`,
`std.convert`, `std.json`, `std.http.request`, `std.http.server`, `std.file`,
`std.path`, `std.env`, `std.events`, `std.error`, `std.time`, `std.log`,
`std.cli`, `std.net`, `std.ffi`.

Most stdlib functions are also available as bare builtins where the checker
imports them globally, but module-qualified calls are preferred in examples when
the module namespace improves clarity.

## Extern (FFI)

```
extern sqlite3
    type Db
    type Statement
    def open(path: String) -> Db
    def close(db: Db) -> Int
    def exec(db: Db, sql: String) -> Int
end
```

Extern blocks declare C library interfaces. Functions inside use the inline parameter syntax (not `@param`/`@return`).

## Testing

```
test 'math operations'
    it 'adds numbers'
        let result = add(1, 2)
        assert.equal(result, 3)
    end

    it 'handles negatives'
        assert.equal(add(-1, 1), 0)
    end
end
```

### Lifecycle hooks

`beforeAll`, `afterAll`, `beforeEach`, and `afterEach` run setup and teardown code around test cases:

```
test 'database'
    beforeAll
        setupDb()
    end

    afterAll
        teardownDb()
    end

    beforeEach
        clearTables()
    end

    afterEach
        resetState()
    end

    it 'inserts a row'
        ...
    end
end
```

- `beforeAll` / `afterAll` — run once before/after all `it` cases in the block
- `beforeEach` / `afterEach` — run before/after each individual `it` case
