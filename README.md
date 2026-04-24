# forai

forai is a programming language built around a simple idea: code should be easy
for humans to review, easy for tools to understand, and easy to ship.

It has a CLI-first workflow, mandatory test gates in the normal run/build
pipeline, built-in documentation, value semantics, a readable contract syntax,
and direct compilation to WebAssembly.

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

def main
    @return Void
do
  print(add(20, 22))
end
```

```bash
fai run main.fai
```

The default pipeline is:

```text
fmt -> check -> test -> run/build
```

If formatting, type checking, or tests fail, the command stops.

## Why forai?

### Built for AI

forai is designed for a world where humans and AI agents both read, write, and
review code.

- **Mandatory testing in the workflow** - `run` and `build` go through tests first.
- **Built-in docs** - function contracts and doc comments are part of the language.
- **Built-in MCP server** - expose the CLI and project knowledge directly to AI tools.
- **Strong CLI tooling** - format, check, test, run, build, docs, interface output, and MCP support.
- **WebAssembly output** - forai compiles to a well-known portable runtime target.
- **Structured syntax** - code is easier for tools to parse, transform, and explain.

### Easy for humans to review

forai favors explicit, boring-on-purpose syntax where the important parts are
visible at the call site and declaration site.

- **Function contracts are obvious** - params and returns are listed before the body.
- **Value semantics reduce aliasing surprises** - assignments copy values.
- **`let` and `var` make mutation visible** - immutable by default, mutable when explicit.
- **What it reads like is what it does** - no hidden class model, no implicit receivers, no macro-heavy control flow.
- **Smaller review surface** - less ambient complexity means reviewers can focus on behavior.

## Language Highlights

### Explicit Function Contracts

```fai
# Create a display name.
def displayName
    @param first String
    @param last String
    @return String
do
  first + ' ' + last
end
```

Named functions use `@param`, `@return`, and `do...end`. Doc comments are
required for named functions except `main`.

### Value Semantics

```fai
var a = User(name: 'Alice', age: 30)
var b = a
b.age = 31

print(a.age)  # 30
print(b.age)  # 31
```

Variables own their values. Assignment copies. Function parameters are immutable
copies unless marked `mutable`.

### Clear Mutability

```fai
let frozen = User(name: 'Alice')
var editable = User(name: 'Bob')

editable.name = 'Robert'  # OK
frozen.name = 'Alicia'    # Error
```

`let` is immutable. `var` allows reassignment and field/index mutation.

### First-Class Tests

```fai
# Multiply two numbers.
def multiply
    @param a Int
    @param b Int
    @return Int
do
  a * b
end

test multiply
beforeEach
  print('checking multiply')
end

it 'multiplies'
  assert.equals(multiply(6, 7), 42)
end
end
```

Tests live next to the code and are driven by the same CLI that formats, checks,
runs, and builds the program.

### Closures and UFCS

```fai
use std.array

def main
    @return Void
do
  let doubled = array.map([1 2 3]) do with n Int
    n * 2
  end

  for n in doubled
    print(n)
  end
end
```

forai supports anonymous `do...end` closures, trailing closures, named callback
types with `type def`, and UFCS-style calls like `value.foo()` when `foo(value)`
is in scope.

### Types, Defaults, and Optional Values

```fai
type Address
  city String = 'Paris'
end

type Person
  name String
  address Address = Address()
  nickname String? = null
end

def main
    @return Void
do
  let p = Person(name: 'Alice')
  print(p.address.city)
  print(p.nickname == null)
end
```

Types are lightweight data declarations with default fields, nested defaults,
optional fields, and generic type parameters.

## CLI

```bash
fai fmt file.fai
fai check file.fai
fai test file.fai
fai run file.fai
fai build [target]
fai doc [query]
fai interface file.fai
fai mcp
```

The binary is available as `fai` / `forai`.

```bash
forai main.fai
```

is shorthand for:

```bash
forai run main.fai
```

## Built-In MCP Server

forai ships with an MCP server:

```bash
fai mcp
```

That means AI development tools can interact with the language through the same
official interface humans use: formatting, checking, testing, building, examples,
documentation, and project metadata. The goal is not a separate AI-only layer.
The goal is one toolchain that works well for humans and agents.

## WebAssembly

forai compiles directly from the checked AST to WebAssembly. The CLI can run the
generated WASM with its host runner, emit standalone `.wasm`, generate browser
HTML output, or package native-style executables with embedded WASM.

```bash
fai build main.fai
fai build --html main.fai
fai run main.wasm
```

## Standard Library

forai includes standard modules for common application work:

- `std.string`
- `std.array`
- `std.dictionary`
- `std.math`
- `std.convert`
- `std.json`
- `std.http.request`
- `std.http.server`
- `std.file`
- `std.path`
- `std.error`
- `std.time`
- `std.log`
- `std.cli`
- `std.net`
- `std.ffi`

Browse docs from the CLI:

```bash
fai doc std
fai doc std.array
fai doc string.contains
```

## FFI and Fullstack Work

forai supports `extern` blocks for C FFI:

```fai
extern sqlite3
  type Db
  def open(path: String) -> Db
  def close(db: Db) -> Int
end
```

It also has early fullstack/RPC support with `remote def`, generated `Server`
proxies, browser-oriented WASM output, and MCP integration for agent-assisted
development.

## Repository Layout

```text
crates/fai-parser         lexer, parser, native AST
crates/fai-compiler       source prep, modules, synthetic modules, metadata
crates/fai-checker        type checker and stdlib metadata
crates/fai-codegen-wasm   direct AST-to-WASM backend
crates/fai-cli            CLI, formatter, docs, runner, project tooling
crates/fai-core           shared value/type infrastructure
crates/fai-ffi            C FFI support
crates/fai-feature-tests  language fixture harness
tests/fixtures/language   end-to-end language feature tests
```

## Working On The Compiler

```bash
cargo test --workspace
cargo test -p fai-feature-tests -- --nocapture
cargo run --bin fai -- --help
```

Language behavior should be covered with fixtures under
`tests/fixtures/language`.

## Status

forai is under active development. The core language, formatter, checker, test
harness, CLI workflow, direct WASM backend, FFI path, and many stdlib features
are implemented, but the language and ecosystem are still evolving.

If the premise sounds useful, try it, break it, and help shape it.

## License

Apache-2.0. See [LICENSE](LICENSE).
