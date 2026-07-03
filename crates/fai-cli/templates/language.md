# forai Language Reference

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
