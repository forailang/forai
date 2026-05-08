# fai - The forai Language Implementation

This is the core implementation of the forai programming language. The CLI compiles forai source directly to WebAssembly through the direct AST-to-WASM backend and runs it with the host WASM runner.

## Language Reference

See [language.md](language.md) for the full language syntax and features.

## Build and Test

```bash
cargo test --workspace
cargo test -p fai-feature-tests -- --nocapture
cargo run --bin fai -- fmt file.fai
cargo run --bin fai -- check file.fai
cargo run --bin fai -- test file.fai
cargo run --bin fai -- run file.fai
cargo run --bin fai -- build [target]
cargo run --bin fai -- doc [query]
```

Pipeline commands run prerequisites in order:

```text
fmt -> check -> test -> run/build
```

The CLI binary is available as `fai` / `forai`. `forai <file.fai>` is shorthand for `forai run <file.fai>`.

## Crate Structure

- **fai-parser** - Lexer, tokenizer, parser, and native AST.
- **fai-compiler** - Source preparation, serde AST conversion, module/dependency resolution, synthetic modules, named params, UFCS metadata, and FFI metadata.
- **fai-checker** - Type checker. Validates types, generics, mutability, doc requirements, imports, UFCS calls, named parameter reorderings, stdlib metadata, and test syntax.
- **fai-codegen-wasm** - Direct AST-to-WASM code generator plus runtime helper module layout.
- **fai-cli** - Main user interface. Commands include `fmt`, `check`, `test`, `run`, `build`, `new`, `doc`, `interface`, and `mcp`. Multi-target builds (`fai.toml` `[project.<name>]` sub-projects) go through a topological build planner that honors `required_targets`, copies declared `[project.<name>.assets]` into the target's `build_dir` after each build, and runs the produced `.wasm` from inside that dir so program-relative paths resolve against the deploy unit.
- **fai-core** - Shared runtime value/type infrastructure.
- **fai-ffi** - Foreign function interface support for calling C libraries.
- **fai-feature-tests** - End-to-end language fixture harness under `tests/fixtures/language`.

## Key Language Concepts

- **`let` / `var`** - `let` is immutable; `var` allows reassignment plus field/index mutation.
- **Value semantics** - Variables own their values. Assignments and parameter passing use copy/value semantics unless a parameter is marked `mutable`.
- **Functions** - Named functions use `@type`, `@param`, `@return`, and `do...end` contract syntax. A doc comment is required above named functions except `main`.
- **Mutable params** - `@param x T, mutable` allows in-place mutation and requires the caller to pass a `var` binding.
- **`type def`** - Named function types for callbacks, event handlers, and closure-typed fields.
- **`@type`** - Generic type parameters on functions and types.
- **UFCS** - `x.foo(args)` rewrites to `foo(x, args)` when `foo` is not a real field.
- **`do...end` blocks** - Anonymous closures. Trailing closures are supported when the last parameter is a `type def`.
- **Modules** - Directories of `.fai` files are imported by directory/module name. Declarations are public by default; `private:` marks following declarations private.
- **Testing** - `test`, `it`, `beforeAll`, `afterAll`, `beforeEach`, and `afterEach` are part of the language test surface.
- **Interop** - `extern` blocks declare C FFI functions. Remote/fullstack projects use `remote def` and generated `Server` proxies.

## Test Conventions

- Rust unit tests live in each crate's `src/` tree.
- Language feature fixtures live in `tests/fixtures/language`.
- forai source in tests uses the contract syntax (`@param` / `@return` / `do...end`).
- Add or update language fixtures for user-visible language behavior.
- Run `cargo test --workspace` before handing off broad changes.

## Repository Notes

- `plans/` contains local planning notes and is intentionally gitignored.
- `target/` and generated WASM artifacts are intentionally gitignored.
