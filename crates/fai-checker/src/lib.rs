//! FAI type checker — validates types before bytecode compilation.

pub mod builtins;
pub mod checker;
pub mod environment;
pub mod error;
pub mod std_modules;
pub mod types;

pub use checker::{Checker, PreparedModule};
pub use error::CheckError;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Helper: check if source type-checks successfully.
    fn check_ok(source: &str) {
        let prepared = fai_compiler::prepare_source(source, None)
            .unwrap_or_else(|e| panic!("prepare error: {}", e));
        let mut checker = Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .unwrap_or_else(|e| panic!("unexpected type error: {}", e.message));
    }

    /// Helper: check that source produces a type error containing the given substring.
    fn check_err(source: &str, expected_msg: &str) {
        let prepared = fai_compiler::prepare_source(source, None)
            .unwrap_or_else(|e| panic!("prepare error: {}", e));
        let mut checker = Checker::new();
        let err = checker
            .check_program(&prepared.serde_ast.statements)
            .expect_err(&format!(
                "expected type error containing '{}' but check passed",
                expected_msg
            ));
        assert!(
            err.message
                .to_lowercase()
                .contains(&expected_msg.to_lowercase()),
            "expected error containing '{}', got: '{}'",
            expected_msg,
            err.message
        );
    }

    fn check_ok_with_root(source_path: &str, source_root: &str) {
        let source = fs::read_to_string(source_path).unwrap();
        let prepared = fai_compiler::prepare_source(&source, Some(source_root))
            .unwrap_or_else(|e| panic!("prepare error: {}", e));
        let prepared_modules: Vec<PreparedModule> = prepared
            .modules
            .iter()
            .map(|m| PreparedModule {
                name: m.name.clone(),
                statements: m.statements.clone(),
                file_paths: Vec::new(),
                private_names: m.private_names.clone(),
                file_path: None,
            })
            .collect();
        let mut checker = Checker::new();
        checker
            .check_with_modules(&prepared.serde_ast.statements, &prepared_modules)
            .unwrap_or_else(|e| panic!("unexpected type error: {}", e.message));
    }

    // ── Correct programs ────────────────────────────────────────

    #[test]
    fn test_check_arithmetic() {
        check_ok("def main\n    @return Int\ndo\n  1 + 2 * 3\nend");
    }

    #[test]
    fn test_checker_records_expression_type_by_location() {
        let source = "def main\n    @return Int\ndo\n  let x = 1 + 2\n  x\nend";
        let prepared = fai_compiler::prepare_source(source, None)
            .unwrap_or_else(|e| panic!("prepare error: {}", e));
        let mut checker = Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .unwrap_or_else(|e| panic!("unexpected type error: {}", e.message));

        let main = prepared
            .serde_ast
            .statements
            .iter()
            .find_map(|stmt| match stmt {
                fai_compiler::ast::Statement::FunctionDeclaration(fd) if fd.name == "main" => {
                    Some(fd)
                }
                _ => None,
            })
            .expect("main function should be present");
        let let_expr = match &main.body[0] {
            fai_compiler::ast::Statement::LetStatement(stmt) => &stmt.value,
            other => panic!("expected let statement, got {other:?}"),
        };
        let key = crate::checker::expression_key(let_expr, String::new());

        assert!(
            matches!(
                checker.expression_types.get(&key),
                Some(crate::types::Type::Int)
            ),
            "expected Int expression type at {:?}",
            key,
        );
    }

    #[test]
    fn test_check_function_call() {
        check_ok("# Add two numbers.\ndef add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n  a + b\nend\n\ndef main\n    @return Int\ndo\n  add(1, 2)\nend");
    }

    #[test]
    fn test_check_string_operations() {
        check_ok("def main\n    @return String\ndo\n  let s = 'hello'\n  print(s)\n  'done'\nend");
    }

    #[test]
    fn test_check_optional_types() {
        check_ok("def main\n    @return String\ndo\n  let x String? = null\n  let y String? = 'hello'\n  'done'\nend");
    }

    #[test]
    fn test_check_for_loop_array() {
        check_ok("def main\n    @return String\ndo\n  let items = [1 2 3]\n  for i in items\n    print(i)\n  end\n  'done'\nend");
    }

    #[test]
    fn test_check_while_loop() {
        check_ok("def main\n    @return String\ndo\n  var x = 0\n  while x < 10\n    x = x + 1\n  end\n  'done'\nend");
    }

    #[test]
    fn test_check_type_declaration() {
        check_ok("type Point\n  x Int\n  y Int\nend\n\ndef main\n    @return String\ndo\n  let p = Point(x: 1, y: 2)\n  print(p.x)\n  'done'\nend");
    }

    #[test]
    fn test_check_enum() {
        check_ok("enum Color\n  red\n  green\nend\n\ndef main\n    @return String\ndo\n  let c = Color.red\n  'done'\nend");
    }

    // ── Type errors ─────────────────────────────────────────────

    #[test]
    fn test_check_err_wrong_return_type() {
        check_err("def main\n    @return Int\ndo\n  'hello'\nend", "return");
    }

    #[test]
    fn test_check_err_while_needs_bool() {
        check_err(
            "def main\n    @return String\ndo\n  while 42\n    print('x')\n  end\n  'done'\nend",
            "Bool",
        );
    }

    #[test]
    fn test_check_err_for_needs_array() {
        check_err(
            "def main\n    @return String\ndo\n  for i in 42\n    print(i)\n  end\n  'done'\nend",
            "array",
        );
    }

    #[test]
    fn test_check_err_break_outside_loop() {
        check_err(
            "def main\n    @return String\ndo\n  break\n  'done'\nend",
            "loop",
        );
    }

    #[test]
    fn test_check_err_continue_outside_loop() {
        check_err(
            "def main\n    @return String\ndo\n  continue\n  'done'\nend",
            "loop",
        );
    }

    #[test]
    fn test_check_err_too_many_args() {
        check_err("# Identity function.\ndef foo\n    @param a Int\n    @return Int\ndo\n  a\nend\n\ndef main\n    @return Int\ndo\n  foo(1, 2)\nend", "Too many");
    }

    // ── std.ffi ─────────────────────────────────────────────────

    #[test]
    fn test_check_ffi_available() {
        check_ok("use std.ffi\n\ndef main\n    @return Bool\ndo\n  ffi.available('sqlite3')\nend");
    }

    #[test]
    fn test_check_ffi_available_wrong_arg_type() {
        check_err(
            "use std.ffi\n\ndef main\n    @return Bool\ndo\n  ffi.available(42)\nend",
            "String",
        );
    }

    // ── UFCS ────────────────────────────────────────────────────

    // ── Angle-bracket Generics ────────────────────────────

    #[test]
    fn test_generic_function_angle_bracket() {
        check_ok("# Echo a value.\ndef echo\n    @type T\n    @param value T\n    @return T\ndo\n  value\nend\n\ndef main\n    @return Int\ndo\n  echo(42)\nend");
    }

    #[test]
    fn test_generic_function_multi_params() {
        check_ok("# Apply a function to a value.\ndef apply\n    @type T\n    @type U\n    @param value T\n    @param fn (T) -> U\n    @return U\ndo\n  fn(value)\nend\n\n# Shout a string.\ndef shout\n    @param s String\n    @return String\ndo\n  s\nend\n\ndef main\n    @return String\ndo\n  apply('hi', shout)\nend");
    }

    #[test]
    fn test_generic_type() {
        check_ok("type Box\n  @type T\n  value T\nend\n\ndef main\n    @return Int\ndo\n  let b = Box(value: 42)\n  b.value\nend");
    }

    #[test]
    fn test_generic_type_string() {
        check_ok("type Box\n  @type T\n  value T\nend\n\ndef main\n    @return String\ndo\n  let b = Box(value: 'hello')\n  b.value\nend");
    }

    #[test]
    fn test_old_dollar_generics_still_work() {
        check_ok("# Echo a value.\ndef echo\n    @param value $T\n    @return $T\ndo\n  value\nend\n\ndef main\n    @return Int\ndo\n  echo(42)\nend");
    }

    #[test]
    fn test_ufcs_basic() {
        // double(x) called as x.double()
        check_ok("# Double a number.\ndef double\n    @param n Int\n    @return Int\ndo\n  n * 2\nend\n\ndef main\n    @return Int\ndo\n  let x = 5\n  x.double()\nend");
    }

    #[test]
    fn test_ufcs_with_args() {
        // add(a, b) called as a.add(b)
        check_ok("# Add two numbers.\ndef add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n  a + b\nend\n\ndef main\n    @return Int\ndo\n  let x = 5\n  x.add(3)\nend");
    }

    #[test]
    fn test_ufcs_chaining() {
        // double(n) called as 5.double().double()
        check_ok("# Double a number.\ndef double\n    @param n Int\n    @return Int\ndo\n  n * 2\nend\n\ndef main\n    @return Int\ndo\n  5.double().double()\nend");
    }

    #[test]
    fn test_ufcs_field_takes_priority() {
        // If a type has a field 'x', UFCS should not override it
        check_ok("type Point\n  x Int\n  y Int\nend\n\ndef main\n    @return Int\ndo\n  let p = Point(x: 1, y: 2)\n  p.x\nend");
    }

    #[test]
    fn test_ufcs_no_matching_function() {
        // x.nonexistent() should error
        check_err(
            "def main\n    @return Int\ndo\n  let x = 5\n  x.nonexistent()\nend",
            "nonexistent",
        );
    }

    #[test]
    fn test_ufcs_missing_forui_view_import_gives_actionable_error() {
        // When a known Forui.view modifier is used but not imported, the error
        // should tell the developer exactly which import to add.
        check_err(
            "type ViewNode\n  kind String\nend\n\ndef main\n    @return Void\ndo\n  let node = ViewNode(kind: 'Label')\n  node.fontSize(14)\nend",
            "Forui.view",
        );
    }

    #[test]
    fn test_ufcs_missing_forui_signal_import_gives_actionable_error() {
        // Missing signal helper import should name Forui.signal.
        check_err(
            "type Signal\n  value Int\nend\n\ndef main\n    @return Void\ndo\n  let s = Signal(value: 0)\n  s.isLoading()\nend",
            "Forui.signal",
        );
    }

    // ── Named Parameters ─────────────────────────────────────

    #[test]
    fn test_named_params_basic() {
        check_ok("# Greet someone.\ndef greet\n    @param name String\n    @param greeting String\n    @return String\ndo\n  greeting\nend\n\ndef main\n    @return String\ndo\n  greet(greeting: 'hi', name: 'bob')\nend");
    }

    #[test]
    fn test_named_params_mixed() {
        // Positional then named
        check_ok("# Greet someone.\ndef greet\n    @param name String\n    @param greeting String\n    @return String\ndo\n  greeting\nend\n\ndef main\n    @return String\ndo\n  greet('bob', greeting: 'hi')\nend");
    }

    #[test]
    fn test_named_params_positional_after_named_error() {
        check_err("# Greet someone.\ndef greet\n    @param name String\n    @param greeting String\n    @return String\ndo\n  greeting\nend\n\ndef main\n    @return String\ndo\n  greet(greeting: 'hi', 'bob')\nend", "Positional argument after named");
    }

    #[test]
    fn test_named_params_unknown_label_error() {
        check_err("# Greet someone.\ndef greet\n    @param name String\n    @return String\ndo\n  name\nend\n\ndef main\n    @return String\ndo\n  greet(foo: 'bob')\nend", "Unknown parameter");
    }

    #[test]
    fn test_named_params_duplicate_error() {
        check_err("# Greet someone.\ndef greet\n    @param name String\n    @return String\ndo\n  name\nend\n\ndef main\n    @return String\ndo\n  greet(name: 'a', name: 'b')\nend", "Duplicate argument");
    }

    #[test]
    fn test_ufcs_type_mismatch() {
        // greet(s: String) called as 5.greet() should fail (Int != String)
        check_err("# Greet someone.\ndef greet\n    @param s String\n    @return String\ndo\n  s\nend\n\ndef main\n    @return String\ndo\n  5.greet()\nend", "");
    }

    #[test]
    fn test_check_external_package_local_imports_with_namespaced_types() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf =
            std::env::temp_dir().join(format!("fai-checker-package-local-{}", nonce));
        let src = root.join("src");
        let dep_root = root.join("dep");
        let dep_src = dep_root.join("src");
        let helper_dir = dep_src.join("helper");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&helper_dir).unwrap();

        fs::write(
            root.join("fai.toml"),
            format!(
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\nDep = \"file://{}\"\n",
                dep_root.display()
            ),
        )
        .unwrap();
        fs::write(
            dep_root.join("fai.toml"),
            "[project]\nname = \"Dep\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        fs::write(src.join("main.fai"), "use { makeNode } from Dep\nuse { Node } from Dep.helper\n\ndef main\n    @return Node\ndo\n  makeNode()\nend\n").unwrap();
        fs::write(dep_src.join("dep.fai"), "use { Node, makeNodeValue } from helper\n\n# Create a node.\ndef makeNode\n    @return Node\ndo\n  makeNodeValue()\nend\n").unwrap();
        fs::write(helper_dir.join("helper.fai"), "type Node\n  text String\nend\n\n# Create a node value.\ndef makeNodeValue\n    @return Node\ndo\n  Node(text: 'ok')\nend\n").unwrap();

        check_ok_with_root(
            src.join("main.fai").to_str().unwrap(),
            src.to_str().unwrap(),
        );

        let _ = fs::remove_dir_all(root);
    }

    // ── Checker gap tests ───────────────────────────────────────

    #[test]
    fn test_check_index_on_non_array() {
        check_err(
            "def main\n    @return Int\ndo\n  let x = 5\n  x[0]\nend",
            "Cannot index",
        );
    }

    #[test]
    fn test_check_assignment_type_mismatch() {
        check_err(
            "def main\n    @return String\ndo\n  var x = 5\n  x = 'hello'\n  'done'\nend",
            "Cannot assign",
        );
    }

    #[test]
    fn test_check_immutable_reassignment() {
        check_err(
            "def main\n    @return String\ndo\n  let x = 5\n  x = 10\n  'done'\nend",
            "immutable",
        );
    }

    #[test]
    fn test_check_missing_function_args() {
        check_err("# Add two numbers.\ndef foo\n    @param a Int\n    @param b Int\n    @return Int\ndo\n  a + b\nend\n\ndef main\n    @return Int\ndo\n  foo(1)\nend", "Missing");
    }

    #[test]
    fn test_check_duplicate_type_fields() {
        check_err(
            "type Bad\n  x Int\n  x String\nend\n\ndef main\n    @return String\ndo\n  'done'\nend",
            "Duplicate field",
        );
    }

    #[test]
    fn test_check_unknown_module() {
        check_err(
            "use std.nonexistent\n\ndef main\n    @return String\ndo\n  'done'\nend",
            "Unknown",
        );
    }

    // ── Doc comment enforcement ─────────────────────────────────

    /// Helper: check that source produces a specific warning.
    fn check_warns(source: &str, expected_warning: &str) {
        let prepared = fai_compiler::prepare_source(source, None)
            .unwrap_or_else(|e| panic!("prepare error: {}", e));
        let mut checker = Checker::new();
        let _ = checker.check_program(&prepared.serde_ast.statements);
        let has_warning = checker
            .warnings
            .iter()
            .any(|w| w.to_lowercase().contains(&expected_warning.to_lowercase()));
        assert!(
            has_warning,
            "expected warning containing '{}', got: {:?}",
            expected_warning, checker.warnings
        );
    }

    /// Helper: check that source produces no warnings.
    fn check_no_warns(source: &str) {
        let prepared = fai_compiler::prepare_source(source, None)
            .unwrap_or_else(|e| panic!("prepare error: {}", e));
        let mut checker = Checker::new();
        let _ = checker.check_program(&prepared.serde_ast.statements);
        assert!(
            checker.warnings.is_empty(),
            "expected no warnings, got: {:?}",
            checker.warnings
        );
    }

    #[test]
    fn test_check_missing_function_doc() {
        // Named function without doc comment is a check error.
        check_err(
            "def add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n  a + b\nend\n\ndef main\n    @return Int\ndo\n  add(1, 2)\nend",
            "missing a required doc comment"
        );
    }

    #[test]
    fn test_missing_doc_comment_error_is_actionable() {
        // Regression test for the agent benchmark: doc-comment errors
        // are the highest-frequency check failure (10-30 instances per
        // bad run). The message must show the exact `# Description.`
        // shape so agents stop inventing Python/JSDoc/rustdoc styles.
        let mut checker = crate::Checker::new();
        let prep = fai_compiler::prepare_source(
            "def add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n  a + b\nend\n",
            None,
        )
        .unwrap();
        let err = checker
            .check_program(&prep.serde_ast.statements)
            .expect_err("missing doc comment should fail");
        assert!(
            err.message.contains("# Description.")
                || err.message.contains("# What this function does"),
            "error should show the `# Description.` paste-ready example, got:\n{}",
            err.message
        );
        assert!(
            err.message.contains("`main` is the only exemption")
                || err.message.contains("main is the only exemption"),
            "error should call out main exemption so agents don't paper-fix main, got:\n{}",
            err.message
        );
        assert!(
            err.message.contains("remote def"),
            "error should remind that `remote def` and `test` need doc comments too, got:\n{}",
            err.message
        );
    }

    #[test]
    fn test_check_main_no_doc_ok() {
        // main is exempt from doc requirement
        check_no_warns("def main\n    @return String\ndo\n  'done'\nend");
    }

    #[test]
    fn test_check_new_def_with_doc_no_warning() {
        // New syntax with doc comment should not warn
        check_no_warns("# Add two numbers.\ndef add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n    a + b\nend\n\ndef main\n    @return Int\ndo\n  add(1, 2)\nend");
    }

    #[test]
    fn test_check_new_def_without_doc_errors() {
        // v2 (@param/@return/do) syntax without doc comment — same
        // required-doc rule applies. Was a warning; now an error.
        check_err(
            "def add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n    a + b\nend\n\ndef main\n    @return Int\ndo\n  add(1, 2)\nend",
            "missing a required doc comment"
        );
    }

    #[test]
    fn test_check_new_syntax_no_deprecation() {
        // New v2 syntax should NOT emit deprecation warning
        check_no_warns("# Add numbers.\ndef add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n    a + b\nend\n\ndef main\n    @return Int\ndo\n  add(1, 2)\nend");
    }

    // ── var mutability enforcement ──────────────────────────────

    #[test]
    fn test_check_let_field_mutation_error() {
        check_err("type P\n  x Int\nend\n\ndef main\n    @return Void\ndo\n  let p = P(x: 1)\n  p.x = 2\nend", "immutable");
    }

    #[test]
    fn test_check_var_field_mutation_ok() {
        check_ok("type P\n  x Int\nend\n\ndef main\n    @return Void\ndo\n  var p = P(x: 1)\n  p.x = 2\nend");
    }

    #[test]
    fn test_check_let_index_mutation_error() {
        check_err(
            "def main\n    @return Void\ndo\n  let items = [1 2 3]\n  items[0] = 99\nend",
            "immutable",
        );
    }

    #[test]
    fn test_check_var_index_mutation_ok() {
        check_ok("def main\n    @return Void\ndo\n  var items = [1 2 3]\n  items[0] = 99\nend");
    }

    // ── mutable param enforcement ───────────────────────────────

    #[test]
    fn test_check_mutable_param_allows_mutation() {
        check_ok("type P\n  x Int\nend\n\n# Mutate.\ndef mutate\n    @param p P, mutable\n    @return Void\ndo\n  p.x = 2\nend\n\ndef main\n    @return Void\ndo\n  var p = P(x: 1)\n  mutate(p)\nend");
    }

    #[test]
    fn test_check_non_mutable_param_blocks_mutation() {
        check_err("type P\n  x Int\nend\n\n# Mutate.\ndef mutate\n    @param p P\n    @return Void\ndo\n  p.x = 2\nend\n\ndef main\n    @return Void\ndo\n  var p = P(x: 1)\n  mutate(p)\nend", "immutable");
    }

    #[test]
    fn test_check_let_passed_to_mutable_param_error() {
        check_err("type P\n  x Int\nend\n\n# Mutate.\ndef mutate\n    @param p P, mutable\n    @return Void\ndo\n  p.x = 2\nend\n\ndef main\n    @return Void\ndo\n  let p = P(x: 1)\n  mutate(p)\nend", "Cannot pass immutable");
    }

    #[test]
    fn test_check_var_passed_to_mutable_param_ok() {
        check_ok("type P\n  x Int\nend\n\n# Mutate.\ndef mutate\n    @param p P, mutable\n    @return Void\ndo\n  p.x = 2\nend\n\ndef main\n    @return Void\ndo\n  var p = P(x: 1)\n  mutate(p)\nend");
    }

    // ── closure captures var bindings — mutable propagation ─────────

    #[test]
    fn test_closure_captures_var_as_mutable_for_mutable_param() {
        // A var binding from the outer scope captured inside a do...end closure
        // must remain mutable so it can be passed to mutable parameters.
        check_ok(
            r#"
type Counter
  count Int
end

# Increment counter.
def increment
    @param c Counter, mutable
    @return Void
do
  c.count = c.count + 1
end

type def Action
    @return Void
end

# Run an action.
def run
    @param action Action
    @return Void
do
  action()
end

def main
    @return Void
do
  var c = Counter(count: 0)
  run(do
    c.increment()
  end)
end
"#,
        );
    }

    #[test]
    fn test_closure_captures_var_ufcs_mutable_param() {
        // Same as above but uses UFCS syntax: c.increment() inside a closure.
        check_ok(
            r#"
type Signal
  value Int
end

# Set value.
def setValue
    @param s Signal, mutable
    @param v Int
    @return Void
do
  s.value = v
end

type def ClickAction
    @return Void
end

# Bind a click handler.
def onClick
    @param action ClickAction
    @return Void
do
  action()
end

def main
    @return Void
do
  var sig = Signal(value: 0)
  onClick(do
    sig.setValue(42)
  end)
end
"#,
        );
    }

    #[test]
    fn test_mutable_param_captured_in_closure_allows_mutable_call() {
        // A mutable function parameter captured in a closure should remain mutable
        // so it can be passed to other mutable-param functions inside the closure.
        check_ok(
            r#"
type Signal
  value Int
end

# Set value.
def setValue
    @param s Signal, mutable
    @param v Int
    @return Void
do
  s.value = v
end

type def ClickAction
    @return Void
end

# Bind a click handler.
def onClick
    @param action ClickAction
    @return Void
do
  action()
end

# Handler function with a mutable Signal param.
def handler
    @param sig Signal, mutable
    @return Void
do
  onClick(do
    sig.setValue(99)
  end)
end

def main
    @return Void
do
  var s = Signal(value: 0)
  handler(s)
end
"#,
        );
    }

    #[test]
    fn test_non_mutable_param_in_closure_blocked_for_mutable_call() {
        // A non-mutable function parameter captured in a closure must be
        // rejected when passed to a mutable-param function.
        check_err(
            r#"
type Signal
  value Int
end

# Set value.
def setValue
    @param s Signal, mutable
    @param v Int
    @return Void
do
  s.value = v
end

type def ClickAction
    @return Void
end

# Bind a click handler.
def onClick
    @param action ClickAction
    @return Void
do
  action()
end

# Handler function WITHOUT mutable — sig is immutable inside.
def handler
    @param sig Signal
    @return Void
do
  onClick(do
    sig.setValue(99)
  end)
end

def main
    @return Void
do
  var s = Signal(value: 0)
  handler(s)
end
"#,
            "Cannot pass immutable",
        );
    }

    #[test]
    fn test_let_captured_in_closure_still_blocked_for_mutable_param() {
        // let bindings captured in closures should still be blocked from mutable params.
        check_err(
            r#"
type Counter
  count Int
end

# Increment counter.
def increment
    @param c Counter, mutable
    @return Void
do
  c.count = c.count + 1
end

type def Action
    @return Void
end

# Run an action.
def run
    @param action Action
    @return Void
do
  action()
end

def main
    @return Void
do
  let c = Counter(count: 0)
  run(do
    c.increment()
  end)
end
"#,
            "Cannot pass immutable",
        );
    }

    // ── Named imports from std modules ───────────────────────────────

    #[test]
    fn test_named_import_from_std_module_ok() {
        check_ok("use { floor } from std.math\n\ndef main\n    @return Int\ndo\n  floor(3.7)\nend");
    }

    #[test]
    fn test_named_import_nonexistent_export_errors() {
        check_err(
            "use { nonexistent } from std.math\n\ndef main\n    @return Void\ndo\n  'done'\nend",
            "does not export",
        );
    }

    #[test]
    fn test_named_import_multiple_from_std_ok() {
        check_ok(
            "use { floor, ceil } from std.math\n\ndef main\n    @return Int\ndo\n  floor(3.7)\nend",
        );
    }

    #[test]
    fn test_glob_import_from_std_module_ok() {
        check_ok("use * from std.math\n\ndef main\n    @return Int\ndo\n  floor(3.7)\nend");
    }

    // ── Multi-return functions ───────────────────────────────────────

    #[test]
    fn test_multi_return_function_ok() {
        check_ok(concat!(
            "# Returns two values.\n",
            "def pair\n",
            "    @return Int\n",
            "    @return String\n",
            "do\n",
            "  1, 'hello'\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let a, b = pair()\n",
            "end\n",
        ));
    }

    #[test]
    fn test_multi_return_wrong_count_errors() {
        check_err(
            concat!(
                "# Returns two values.\n",
                "def pair\n",
                "    @return Int\n",
                "    @return String\n",
                "do\n",
                "  1, 'hello'\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "  let a, b, c = pair()\n",
                "end\n",
            ),
            "values",
        );
    }

    #[test]
    fn test_multi_assign_to_single_binding_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 1, 2\nend",
            "Cannot assign multiple",
        );
    }

    // ── Range expression ─────────────────────────────────────────────

    #[test]
    fn test_range_non_int_bounds_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  for i in 1.5..3.5\n    print(i)\n  end\nend",
            "Range expression requires Int",
        );
    }

    // ── Array type errors ────────────────────────────────────────────

    #[test]
    fn test_array_mixed_types_widen_to_unknown() {
        // forai#1: mixed-type literals widen to `Unknown[]` instead of
        // erroring. Sqlite param arrays and RPC arg arrays both rely on
        // this — the runtime is happy with mixed-type arrays, the
        // checker should not preemptively reject them.
        check_ok("def main\n    @return Void\ndo\n  let a Unknown[] = [1 'hello' 3]\nend");
    }

    // ── Optional check and force unwrap errors ───────────────────────

    #[test]
    fn test_optional_check_on_non_optional_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 5\n  let ok = x?\nend",
            "Optional check requires an optional",
        );
    }

    #[test]
    fn test_force_unwrap_on_non_optional_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 5\n  let y = x!\nend",
            "Force unwrap requires an optional",
        );
    }

    #[test]
    fn test_force_unwrap_on_optional_ok() {
        check_ok("def main\n    @return String\ndo\n  let x String? = 'hello'\n  x!\nend");
    }

    #[test]
    fn test_optional_check_ok() {
        check_ok("def main\n    @return Bool\ndo\n  let x Int? = 5\n  x?\nend");
    }

    // ── Binary operator type errors ──────────────────────────────────

    #[test]
    fn test_binary_and_requires_bool() {
        check_err("def main\n    @return Bool\ndo\n  1 and 2\nend", "Bool");
    }

    #[test]
    fn test_binary_or_requires_bool() {
        check_err("def main\n    @return Bool\ndo\n  1 or 2\nend", "Bool");
    }

    #[test]
    fn test_binary_and_bool_operands_ok() {
        check_ok("def main\n    @return Bool\ndo\n  true and false\nend");
    }

    #[test]
    fn test_binary_add_string_concat_ok() {
        check_ok("def main\n    @return String\ndo\n  'hello' + ' world'\nend");
    }

    #[test]
    fn test_binary_add_mixed_types_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = true + 1\nend",
            "requires numeric or string",
        );
    }

    // ── Numeric literal narrowing ────────────────────────────────────
    //
    // Rule: Int → Float widens unconditionally (Int is a subset of
    // Float). Float → Int is allowed only when the right-hand side is a
    // literal whose value is whole (e.g. `1.0`); non-whole literals
    // and non-literal Float values are rejected.

    #[test]
    fn test_let_int_annotated_int_literal_ok() {
        check_ok("def main\n    @return Void\ndo\n  let v Int = 1\nend");
    }

    #[test]
    fn test_let_int_annotated_whole_float_literal_ok() {
        check_ok("def main\n    @return Void\ndo\n  let v Int = 1.0\nend");
        check_ok("def main\n    @return Void\ndo\n  let v Int = 42.0\nend");
    }

    #[test]
    fn test_let_int_annotated_nonwhole_float_literal_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let v Int = 1.23\nend",
            "Cannot assign Float to Int",
        );
    }

    #[test]
    fn test_let_int_annotated_float_variable_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let f = 1.5\n  let n Int = f\nend",
            "Cannot assign Float to Int",
        );
    }

    #[test]
    fn test_let_float_annotated_int_literal_widens() {
        check_ok("def main\n    @return Void\ndo\n  let v Float = 1\nend");
        check_ok("def main\n    @return Void\ndo\n  let v Float = 42\nend");
    }

    #[test]
    fn test_let_float_annotated_float_literal_ok() {
        check_ok("def main\n    @return Void\ndo\n  let v Float = 1.0\nend");
        check_ok("def main\n    @return Void\ndo\n  let v Float = 1.23\nend");
    }

    #[test]
    fn test_binary_subtract_non_numeric_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 'hello' - 1\nend",
            "requires numeric",
        );
    }

    #[test]
    fn test_binary_multiply_non_numeric_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 'a' * 3\nend",
            "requires numeric",
        );
    }

    #[test]
    fn test_binary_modulo_non_numeric_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 'a' % 3\nend",
            "requires numeric",
        );
    }

    #[test]
    fn test_binary_floor_div_non_numeric_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 'a' // 3\nend",
            "requires numeric",
        );
    }

    #[test]
    fn test_binary_pow_non_numeric_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 'a' ** 2\nend",
            "requires numeric",
        );
    }

    #[test]
    fn test_binary_ordering_rejects_non_numeric_non_string() {
        check_err(
            "def main\n    @return Bool\ndo\n  true > false\nend",
            "numeric or string",
        );
    }

    #[test]
    fn test_binary_division_returns_float() {
        check_ok("def main\n    @return Float\ndo\n  10 / 3\nend");
    }

    #[test]
    fn test_binary_comparison_incompatible_types_errors() {
        check_err(
            "def main\n    @return Bool\ndo\n  1 == 'hello'\nend",
            "Cannot compare",
        );
    }

    #[test]
    fn test_binary_int_comparison_ok() {
        check_ok("def main\n    @return Bool\ndo\n  1 < 2\nend");
    }

    // ── Unary operator errors ─────────────────────────────────────────

    #[test]
    fn test_unary_not_requires_bool_errors() {
        check_err(
            "def main\n    @return Bool\ndo\n  !42\nend",
            "Unary operator '!' requires a Bool",
        );
    }

    #[test]
    fn test_unary_neg_requires_numeric_errors() {
        check_err(
            "def main\n    @return String\ndo\n  -'hello'\nend",
            "Unary operator '-' requires a numeric",
        );
    }

    #[test]
    fn test_unary_not_bool_ok() {
        check_ok("def main\n    @return Bool\ndo\n  !true\nend");
    }

    #[test]
    fn test_unary_neg_int_ok() {
        check_ok("def main\n    @return Int\ndo\n  -5\nend");
    }

    // ── If/case branch type consistency ───────────────────────────────

    #[test]
    fn test_if_inconsistent_branch_types_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  if true\n    1\n  else\n    'hello'\n  end\nend",
            "branches must return the same type",
        );
    }

    #[test]
    fn test_if_without_else_ok() {
        check_ok("def main\n    @return Void\ndo\n  if true\n    print('yes')\n  end\nend");
    }

    #[test]
    fn test_if_without_else_non_bool_condition_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  if 42\n    print('yes')\n  end\nend",
            "Bool",
        );
    }

    #[test]
    fn test_case_match_type_mismatch_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 5\n  case x\n  when 'hello'\n    print('match')\n  end\nend",
            "match type",
        );
    }

    #[test]
    fn test_case_inconsistent_branch_types_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 5\n  case x\n  when 1\n    'a'\n  default\n    42\n  end\nend",
            "branches must return the same type",
        );
    }

    // ── Type construction errors ──────────────────────────────────────

    #[test]
    fn test_type_construction_unlabeled_args_errors() {
        check_err(
            "type Point\n  x Int\n  y Int\nend\n\ndef main\n    @return Void\ndo\n  let p = Point(1, 2)\nend",
            "requires labeled arguments",
        );
    }

    #[test]
    fn test_type_construction_wrong_field_type_errors() {
        check_err(
            "type Point\n  x Int\n  y Int\nend\n\ndef main\n    @return Void\ndo\n  let p = Point(x: 'hello', y: 2)\nend",
            "Field 'x' on 'Point' expects Int",
        );
    }

    // ── Member access errors ──────────────────────────────────────────

    #[test]
    fn test_unknown_field_on_type_errors() {
        check_err(
            "type Point\n  x Int\n  y Int\nend\n\ndef main\n    @return Void\ndo\n  let p = Point(x: 1, y: 2)\n  print(p.z)\nend",
            "Unknown field 'z' on type 'Point'",
        );
    }

    #[test]
    fn test_property_access_on_primitive_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 5\n  let y = x.foo\nend",
            "Cannot access property",
        );
    }

    #[test]
    fn test_property_access_on_array_suggests_items_and_getters() {
        // Agents repeatedly write `arr.field` hoping for dict-style lookup.
        // The hint should point them at .items / .length AND at the
        // dictionary getters they likely meant.
        check_err(
            "def main\n    @return Void\ndo\n  let rows = [1, 2, 3]\n  print(rows.teams)\nend",
            "getString(d, 'teams')",
        );
    }

    #[test]
    fn test_property_access_on_string_suggests_module() {
        check_err(
            "def main\n    @return Void\ndo\n  let s = 'hi'\n  let y = s.length\nend",
            "string.length(s)",
        );
    }

    #[test]
    fn test_multi_error_pass_reports_every_issue_in_one_run() {
        // `fai check` must NOT stop at the first error — agents wasted
        // build cycles rediscovering one issue at a time. Three distinct
        // errors here: operator type mismatch inside a fn, wrong call
        // arg type, and a bad array property access. All should surface.
        let src = "# Triple.\ndef triple\n    @param x Int\n    @return Int\ndo\n  x * \"three\"\nend\n\ndef main\n    @return Void\ndo\n  let a = triple(\"hi\")\n  let rows = [1, 2, 3]\n  print(rows.teams)\nend\n";
        let prepared = fai_compiler::prepare_source(src, None)
            .unwrap_or_else(|e| panic!("prepare error: {}", e));
        let mut checker = Checker::new();
        let _ = checker.check_program(&prepared.serde_ast.statements);
        assert!(
            checker.collected_errors.len() >= 3,
            "expected at least 3 errors, got {}: {:?}",
            checker.collected_errors.len(),
            checker
                .collected_errors
                .iter()
                .map(|e| &e.message)
                .collect::<Vec<_>>()
        );
        let joined: Vec<&str> = checker
            .collected_errors
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert!(
            joined.iter().any(|m| m.contains("numeric")),
            "should report the operator error, got: {:?}",
            joined
        );
        assert!(
            joined
                .iter()
                .any(|m| m.contains("triple") || m.contains("Int")),
            "should report the call-arg error, got: {:?}",
            joined
        );
        assert!(
            joined.iter().any(|m| m.contains("teams")),
            "should report the property-access error, got: {:?}",
            joined
        );
    }

    #[test]
    fn test_unknown_enum_member_errors() {
        check_err(
            "enum Color\n  red\n  green\nend\n\ndef main\n    @return Void\ndo\n  let c = Color.purple\nend",
            "Unknown enum member",
        );
    }

    #[test]
    fn test_error_type_field_message_ok() {
        check_ok(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  try\n",
            "    throw 'e'\n",
            "  catch e\n",
            "    print(e.message)\n",
            "  end\n",
            "end\n",
        ));
    }

    #[test]
    fn test_unknown_field_on_error_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  try\n    throw 'e'\n  catch e\n    print(e.nonexistent)\n  end\nend",
            "Unknown field",
        );
    }

    // ── Cannot call non-function ──────────────────────────────────────

    #[test]
    fn test_cannot_call_non_function_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 5\n  x()\nend",
            "Cannot call",
        );
    }

    // ── Index expression errors ───────────────────────────────────────

    #[test]
    fn test_array_index_non_int_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let a = [1 2 3]\n  let x = a['hello']\nend",
            "Array index must be Int",
        );
    }

    #[test]
    fn test_dict_index_ok() {
        check_ok("def main\n    @return Void\ndo\n  let d = {x: 1}\n  let v = d['x']\nend");
    }

    // ── Argument type mismatch ────────────────────────────────────────

    #[test]
    fn test_argument_type_mismatch_errors() {
        check_err(
            concat!(
                "# Greet someone.\n",
                "def greet\n",
                "    @param name String\n",
                "    @return Void\n",
                "do\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "  greet(42)\n",
                "end\n",
            ),
            "Argument 'name'",
        );
    }

    // ── Multiple assignment ───────────────────────────────────────────

    #[test]
    fn test_multiple_assignment_to_multiple_vars_ok() {
        check_ok(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  var a = 1\n",
            "  var b = 2\n",
            "  a, b = 10, 20\n",
            "end\n",
        ));
    }

    #[test]
    fn test_multiple_assignment_single_value_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  var a = 1\n  var b = 2\n  a, b = 5\nend",
            "Multiple assignment",
        );
    }

    // ── Top-level let/var statements ──────────────────────────────────

    #[test]
    fn test_top_level_let_ok() {
        check_ok("let greeting = 'hello'\n\ndef main\n    @return String\ndo\n  greeting\nend");
    }

    #[test]
    fn test_top_level_var_ok() {
        check_ok("var count = 0\n\ndef main\n    @return Int\ndo\n  count\nend");
    }

    // ── throw / try / nowait statements ──────────────────────────────

    #[test]
    fn test_throw_ok() {
        check_ok("def main\n    @return Void\ndo\n  throw 'error'\nend");
    }

    #[test]
    fn test_try_catch_ok() {
        check_ok(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  try\n",
            "    print('ok')\n",
            "  catch e\n",
            "    print(e)\n",
            "  end\n",
            "end\n",
        ));
    }

    #[test]
    fn test_try_catch_finally_ok() {
        check_ok(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  try\n",
            "    print('ok')\n",
            "  catch e\n",
            "    print(e)\n",
            "  finally\n",
            "    print('done')\n",
            "  end\n",
            "end\n",
        ));
    }

    #[test]
    fn test_nowait_ok() {
        check_ok(concat!(
            "# A background task.\n",
            "def task\n",
            "    @return Void\n",
            "do\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  nowait task()\n",
            "end\n",
        ));
    }

    // ── Template strings ──────────────────────────────────────────────

    #[test]
    fn test_template_string_ok() {
        check_ok(
            "def main\n    @return String\ndo\n  let name = 'world'\n  \"hello {{name}}\"\nend",
        );
    }

    // ── Dictionary expressions ────────────────────────────────────────

    #[test]
    fn test_dictionary_expression_ok() {
        check_ok("def main\n    @return Dictionary\ndo\n  let d = {x: 1 y: 2}\n  d\nend");
    }

    // ── Tuple expressions ─────────────────────────────────────────────

    #[test]
    fn test_tuple_expression_ok() {
        check_ok(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let a, b = 1, 'hello'\n",
            "end\n",
        ));
    }

    // ── Function type def ─────────────────────────────────────────────

    #[test]
    fn test_function_type_def_ok() {
        // A named function matching OnClick's signature can be passed as the callback
        check_ok(concat!(
            "# A click callback type.\n",
            "type def OnClick\n",
            "    @param x Int\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "# Run with callback.\n",
            "def run\n",
            "    @param cb OnClick\n",
            "    @return Void\n",
            "do\n",
            "end\n",
            "\n",
            "# Handle click.\n",
            "def handleClick\n",
            "    @param x Int\n",
            "    @return Void\n",
            "do\n",
            "  print(x)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  run(handleClick)\n",
            "end\n",
        ));
    }

    // ── Extern blocks ─────────────────────────────────────────────────

    #[test]
    fn test_extern_block_ok() {
        check_ok(concat!(
            "extern libc\n",
            "  def strlen(s: String) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print('hi')\n",
            "end\n",
        ));
    }

    // ── Response type field access ────────────────────────────────────

    #[test]
    fn test_response_type_fields_ok() {
        check_ok(concat!(
            "use std.http.request\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let resp = request.get('http://example.com')\n",
            "  print(resp.status)\n",
            "  print(resp.body)\n",
            "end\n",
        ));
    }

    // ── Function expressions (closures) ──────────────────────────────

    #[test]
    fn test_function_expression_ok() {
        check_ok(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let double = def(x: Int) -> Int\n",
            "    x * 2\n",
            "  end\n",
            "  double(5)\n",
            "end\n",
        ));
    }

    // ── unwrap with fallback ─────────────────────────────────────────

    #[test]
    fn test_unwrap_with_fallback_ok() {
        check_ok("def main\n    @return Int\ndo\n  let x Int? = null\n  unwrap(x, 0)\nend");
    }

    #[test]
    fn test_unwrap_wrong_fallback_type_errors() {
        check_err(
            "def main\n    @return Int\ndo\n  let x Int? = null\n  unwrap(x, 'wrong')\nend",
            "fallback expects",
        );
    }

    #[test]
    fn test_unwrap_non_optional_errors() {
        check_err(
            "def main\n    @return Int\ndo\n  unwrap(5, 0)\nend",
            "optional as the first argument",
        );
    }

    // ── empty array refinement ────────────────────────────────────────

    #[test]
    fn test_empty_array_typed_binding_ok() {
        check_ok("def main\n    @return Void\ndo\n  let xs Int[] = []\nend");
    }

    // ── all() variadic ───────────────────────────────────────────────

    #[test]
    fn test_all_variadic_ok() {
        check_ok(concat!(
            "# A task.\n",
            "def task\n",
            "    @return Int\n",
            "do\n",
            "  42\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let a, b = all(task(), task())\n",
            "end\n",
        ));
    }

    // ── default param values ─────────────────────────────────────────

    #[test]
    fn test_default_param_value_ok() {
        check_ok(concat!(
            "# Greet someone.\n",
            "def greet\n",
            "    @param name String, default: 'world'\n",
            "    @return String\n",
            "do\n",
            "  'hello ' + name\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  greet()\n",
            "end\n",
        ));
    }

    // ── case with default branch ──────────────────────────────────────

    #[test]
    fn test_case_with_default_ok() {
        check_ok(concat!(
            "def main\n",
            "    @return String\n",
            "do\n",
            "  let x = 5\n",
            "  case x\n",
            "  when 1\n",
            "    'one'\n",
            "  default\n",
            "    'other'\n",
            "  end\n",
            "end\n",
        ));
    }

    // ── type with optional field ──────────────────────────────────────

    #[test]
    fn test_type_with_optional_field_ok() {
        check_ok(concat!(
            "type Config\n",
            "  name String\n",
            "  debug Bool?\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let c = Config(name: 'test')\n",
            "  print(c.name)\n",
            "end\n",
        ));
    }

    // ── std module namespace access errors ───────────────────────────

    #[test]
    fn test_std_module_unknown_export_access_errors() {
        check_err(
            "use std.math\n\ndef main\n    @return Void\ndo\n  math.nonexistent()\nend",
            "Unknown export",
        );
    }

    // ── for loop range ok ────────────────────────────────────────────

    #[test]
    fn test_for_loop_range_ok() {
        check_ok("def main\n    @return Void\ndo\n  for i in 0..9\n    print(i)\n  end\nend");
    }

    // ── break/continue inside loop ok ────────────────────────────────

    #[test]
    fn test_break_inside_loop_ok() {
        check_ok("def main\n    @return Void\ndo\n  while true\n    break\n  end\nend");
    }

    #[test]
    fn test_continue_inside_loop_ok() {
        check_ok("def main\n    @return Void\ndo\n  while true\n    continue\n  end\nend");
    }

    // ── generic function with multiple type params ────────────────────

    #[test]
    fn test_generic_function_type_param_bound_ok() {
        check_ok(concat!(
            "# Map a value.\n",
            "def mapVal\n",
            "    @type T\n",
            "    @type U\n",
            "    @param val T\n",
            "    @param fn (T) -> U\n",
            "    @return U\n",
            "do\n",
            "  fn(val)\n",
            "end\n",
            "\n",
            "# Double.\n",
            "def double\n",
            "    @param n Int\n",
            "    @return Int\n",
            "do\n",
            "  n * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  mapVal(5, double)\n",
            "end\n",
        ));
    }

    // ── if/else consistent types ok ──────────────────────────────────

    #[test]
    fn test_if_else_consistent_types_ok() {
        check_ok("def main\n    @return Int\ndo\n  if true\n    1\n  else\n    2\n  end\nend");
    }

    // ── type def in function signature ───────────────────────────────

    #[test]
    fn test_type_with_type_def_field_ok() {
        check_ok(concat!(
            "# A handler type.\n",
            "type def Handler\n",
            "    @param x Int\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "type Widget\n",
            "  onClick Handler?\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let w = Widget()\n",
            "  print('ok')\n",
            "end\n",
        ));
    }

    #[test]
    fn test_generic_type_field_access_through_function_return() {
        // useSignal returns Signal which has value: $T.
        // When called with a concrete type, field access on .value should
        // resolve to that concrete type, not $T.
        check_ok(concat!(
            "type Signal\n",
            "  id Int\n",
            "  value $T\n",
            "end\n",
            "\n",
            "type AppState\n",
            "  count Int\n",
            "  name String\n",
            "end\n",
            "\n",
            "# Create a signal.\n",
            "def createSignal\n",
            "    @type T\n",
            "    @param initialValue $T\n",
            "    @return Signal\n",
            "do\n",
            "  Signal(id: 0, value: initialValue)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let s = createSignal(AppState(count: 0, name: 'test'))\n",
            "  # Accessing .value should give AppState, not $T\n",
            "  let state = s.value\n",
            "  # Accessing .count on the resolved type should work\n",
            "  print(state.count)\n",
            "  print(state.name)\n",
            "end\n",
        ));
    }

    #[test]
    fn test_generic_type_field_access_across_modules() {
        // Cross-module: Signal+useSignal in one module, app in another.
        // Accessing .value on the returned Signal should resolve generics.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf = std::env::temp_dir().join(format!("fai-checker-generic-{}", nonce));
        let src = root.join("src");
        let sig_dir = src.join("signal");
        fs::create_dir_all(&sig_dir).unwrap();

        fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();

        fs::write(
            sig_dir.join("signal.fai"),
            concat!(
                "type Signal\n",
                "  id Int\n",
                "  value $T\n",
                "end\n",
                "\n",
                "# Create a signal.\n",
                "def createSignal\n",
                "    @type T\n",
                "    @param initialValue $T\n",
                "    @return Signal\n",
                "do\n",
                "  Signal(id: 0, value: initialValue)\n",
                "end\n",
            ),
        )
        .unwrap();

        fs::write(
            src.join("main.fai"),
            concat!(
                "use { Signal, createSignal } from signal\n",
                "\n",
                "type AppState\n",
                "  count Int\n",
                "  name String\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "  var s = createSignal(AppState(count: 0, name: 'test'))\n",
                "  let state = s.value\n",
                "  print(state.count)\n",
                "end\n",
            ),
        )
        .unwrap();

        check_ok_with_root(
            src.join("main.fai").to_str().unwrap(),
            src.to_str().unwrap(),
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_generic_type_field_access_across_packages() {
        // Cross-package: Signal+createSignal in an external package dependency.
        // This is the actual failing scenario (forui Signal imported from Forui.signal).
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf = std::env::temp_dir().join(format!("fai-checker-generic-pkg-{}", nonce));
        let app_src = root.join("src");
        let lib_root = root.join("lib");
        let lib_src = lib_root.join("src");
        let lib_sig = lib_src.join("signal");
        fs::create_dir_all(&app_src).unwrap();
        fs::create_dir_all(&lib_sig).unwrap();

        // Library package with Signal type and createSignal function
        fs::write(
            lib_root.join("fai.toml"),
            "[project]\nname = \"Lib\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        fs::write(
            lib_sig.join("signal.fai"),
            concat!(
                "type Signal\n",
                "  id Int\n",
                "  value $T\n",
                "end\n",
                "\n",
                "# Create a signal.\n",
                "def createSignal\n",
                "    @type T\n",
                "    @param initialValue $T\n",
                "    @return Signal\n",
                "do\n",
                "  Signal(id: 0, value: initialValue)\n",
                "end\n",
            ),
        )
        .unwrap();

        // App package depends on Lib
        fs::write(root.join("fai.toml"), format!(
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\nLib = \"file://{}\"\n",
            lib_root.display())).unwrap();
        fs::write(
            app_src.join("main.fai"),
            concat!(
                "use { Signal, createSignal } from Lib.signal\n",
                "\n",
                "type AppState\n",
                "  count Int\n",
                "  name String\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "  var s = createSignal(AppState(count: 0, name: 'test'))\n",
                "  let state = s.value\n",
                "  print(state.count)\n",
                "end\n",
            ),
        )
        .unwrap();

        check_ok_with_root(
            app_src.join("main.fai").to_str().unwrap(),
            app_src.to_str().unwrap(),
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_generic_type_array_field_iterable_through_function_return() {
        // When a generic function returns a type with an array field,
        // iterating over that field with `for` should work.
        check_ok(concat!(
            "type Container\n",
            "  items $T\n",
            "end\n",
            "\n",
            "# Wrap items.\n",
            "def wrap\n",
            "    @type T\n",
            "    @param items $T\n",
            "    @return Container\n",
            "do\n",
            "  Container(items: items)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let c = wrap([1, 2, 3])\n",
            "  for item in c.items\n",
            "    print(item)\n",
            "  end\n",
            "end\n",
        ));
    }

    // ── named_param_reorder keys must not collide across modules ──────
    //
    // Regression: `Checker::named_param_reorder` used (line, column) as
    // the key, which is a file-local coordinate. With multiple modules,
    // two unrelated calls sharing the same (line, column) in their
    // respective files would collide, and the compiler would apply the
    // wrong reorder — dropping args or reshuffling them. This bit
    // `forui-examples/signup` because a `Form(a, onSubmit: …)` call at
    // `main.fai:23:3` collided with `renderNodeHtml(node, path, true)`
    // at `html-forui/src/html/html.fai:23:3`, stripping the third arg
    // (`true`) from renderNodeHtml and producing wasm that failed to
    // validate with "not enough arguments on the stack for call".
    //
    // The fix: key the reorder map by `(module_name, line, column)` so
    // entries from different files never collide.

    // ── Generic field access through function PARAMS ────────────────────
    //
    // Previously the checker only resolved generic field access through
    // function RETURN types. When a generic struct was passed in via
    // @param, the body lost track of T and any access to a `$T` field
    // produced "$T", which then failed downstream operations like string
    // concat and assignment. This bit `forui-examples/signup` because
    // `ResultView(@param name Signal)` couldn't compute `'Name: ' +
    // name.value` even though name was `useSignal('')`.
    //
    // The intended semantics: when a generic field is accessed and T
    // cannot be locally inferred, the resulting type is `Unknown`
    // (permissive) — NOT the literal `$T` placeholder, which downstream
    // type rules will reject.
    //
    // The block below is a single property — "unbound generic field
    // access doesn't poison downstream typing" — broken into many small
    // tests so a failure points exactly at which shape regressed.

    // ── shape: $T field via param, used in concat ──────────────────────
    #[test]
    fn test_generic_field_param_string_concat_dollar_t() {
        check_ok(concat!(
            "type Box\n",
            "  value $T\n",
            "end\n",
            "\n",
            "# Show.\n",
            "def show\n",
            "    @param b Box\n",
            "    @return Void\n",
            "do\n",
            "  print('val: ' + b.value)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  show(Box(value: 'hi'))\n",
            "end\n",
        ));
    }

    // ── shape: same, with new @type T form ─────────────────────────────
    #[test]
    fn test_generic_field_param_string_concat_at_type() {
        check_ok(concat!(
            "type Box\n",
            "  @type T\n",
            "  value T\n",
            "end\n",
            "\n",
            "# Show.\n",
            "def show\n",
            "    @param b Box\n",
            "    @return Void\n",
            "do\n",
            "  print('val: ' + b.value)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  show(Box(value: 'hi'))\n",
            "end\n",
        ));
    }

    // ── shape: assigned to typed local ─────────────────────────────────
    #[test]
    fn test_generic_field_param_assign_to_typed_let() {
        check_ok(concat!(
            "type Box\n",
            "  value $T\n",
            "end\n",
            "\n",
            "# Pull.\n",
            "def pull\n",
            "    @param b Box\n",
            "    @return Void\n",
            "do\n",
            "  let s String = b.value\n",
            "  print(s)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  pull(Box(value: 'hi'))\n",
            "end\n",
        ));
    }

    // ── shape: passed to another fn ────────────────────────────────────
    #[test]
    fn test_generic_field_param_passed_to_fn() {
        check_ok(concat!(
            "type Box\n",
            "  value $T\n",
            "end\n",
            "\n",
            "# Echo.\n",
            "def echo\n",
            "    @param s String\n",
            "    @return Void\n",
            "do\n",
            "  print(s)\n",
            "end\n",
            "\n",
            "# Hand.\n",
            "def hand\n",
            "    @param b Box\n",
            "    @return Void\n",
            "do\n",
            "  echo(b.value)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  hand(Box(value: 'hi'))\n",
            "end\n",
        ));
    }

    // ── shape: == comparison against concrete value ────────────────────
    #[test]
    fn test_generic_field_param_eq_concrete() {
        check_ok(concat!(
            "type Box\n",
            "  value $T\n",
            "end\n",
            "\n",
            "# Match.\n",
            "def match\n",
            "    @param b Box\n",
            "    @return Bool\n",
            "do\n",
            "  b.value == 'target'\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print(match(Box(value: 'hi')))\n",
            "end\n",
        ));
    }

    // ── shape: returned from fn taking generic param ───────────────────
    #[test]
    fn test_generic_field_param_returned() {
        check_ok(concat!(
            "type Box\n",
            "  value $T\n",
            "end\n",
            "\n",
            "# Read.\n",
            "def read\n",
            "    @param b Box\n",
            "    @return String\n",
            "do\n",
            "  b.value\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print(read(Box(value: 'hi')))\n",
            "end\n",
        ));
    }

    // ── shape: field is Optional<T> ────────────────────────────────────
    #[test]
    fn test_generic_optional_field_param() {
        check_ok(concat!(
            "type Box\n",
            "  value $T?\n",
            "end\n",
            "\n",
            "# Has.\n",
            "def has\n",
            "    @param b Box\n",
            "    @return Bool\n",
            "do\n",
            "  b.value != null\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print(has(Box(value: 'x')))\n",
            "end\n",
        ));
    }

    // ── shape: field is Array<T> ───────────────────────────────────────
    #[test]
    fn test_generic_array_field_param_iter() {
        check_ok(concat!(
            "type Box\n",
            "  items $T[]\n",
            "end\n",
            "\n",
            "# Walk.\n",
            "def walk\n",
            "    @param b Box\n",
            "    @return Void\n",
            "do\n",
            "  for x in b.items\n",
            "    print(x)\n",
            "  end\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  walk(Box(items: [1 2 3]))\n",
            "end\n",
        ));
    }

    // ── shape: nested — Box<Box<T>> via param ──────────────────────────
    #[test]
    fn test_generic_nested_field_param() {
        check_ok(concat!(
            "type Inner\n",
            "  value $T\n",
            "end\n",
            "\n",
            "type Outer\n",
            "  inner Inner\n",
            "end\n",
            "\n",
            "# Pull.\n",
            "def pull\n",
            "    @param o Outer\n",
            "    @return Void\n",
            "do\n",
            "  print('v: ' + o.inner.value)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  pull(Outer(inner: Inner(value: 'hi')))\n",
            "end\n",
        ));
    }

    // ── shape: cross-module via @param ─────────────────────────────────
    #[test]
    fn test_generic_field_param_cross_module() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf = std::env::temp_dir().join(format!("fai-checker-genparam-cm-{}", nonce));
        let src = root.join("src");
        let bx_dir = src.join("box");
        fs::create_dir_all(&bx_dir).unwrap();

        fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();

        fs::write(
            bx_dir.join("box.fai"),
            concat!(
                "type Box\n",
                "  value $T\n",
                "end\n",
                "\n",
                "# Mk.\n",
                "def mk\n",
                "    @type T\n",
                "    @param v $T\n",
                "    @return Box\n",
                "do\n",
                "  Box(value: v)\n",
                "end\n",
            ),
        )
        .unwrap();

        fs::write(
            src.join("main.fai"),
            concat!(
                "use { Box, mk } from box\n",
                "\n",
                "# Show.\n",
                "def show\n",
                "    @param b Box\n",
                "    @return Void\n",
                "do\n",
                "  print('val: ' + b.value)\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "  show(mk('hi'))\n",
                "end\n",
            ),
        )
        .unwrap();

        check_ok_with_root(
            src.join("main.fai").to_str().unwrap(),
            src.to_str().unwrap(),
        );
        let _ = fs::remove_dir_all(root);
    }

    // ── shape: real signup repro — Signal<String> through @param ───────
    //
    // This is the exact shape the signup example hit. Locked in as a
    // test so the bug doesn't silently come back.
    #[test]
    fn test_signal_value_through_param_string_concat() {
        check_ok(concat!(
            "type Signal\n",
            "  id Int\n",
            "  value $T\n",
            "end\n",
            "\n",
            "# Make.\n",
            "def makeSignal\n",
            "    @type T\n",
            "    @param initial $T\n",
            "    @return Signal\n",
            "do\n",
            "  Signal(id: 0, value: initial)\n",
            "end\n",
            "\n",
            "# Show.\n",
            "def show\n",
            "    @param s Signal\n",
            "    @return Void\n",
            "do\n",
            "  print('val: ' + s.value)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  show(makeSignal('alice'))\n",
            "end\n",
        ));
    }

    // ── back-compat: TypeParameter still flows through generic-fn body ─
    //
    // Inside a generic function body, $T is still a TypeParameter (not
    // Unknown). The function gets called with concrete args and the
    // checker resolves T from the call site. This case must NOT regress.
    #[test]
    fn test_generic_function_body_keeps_type_parameter() {
        check_ok(concat!(
            "# Echo.\n",
            "def echo\n",
            "    @type T\n",
            "    @param value $T\n",
            "    @return $T\n",
            "do\n",
            "  value\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let s String = echo('hi')\n",
            "  print(s)\n",
            "end\n",
        ));
    }

    // ── check_with_modules helper ─────────────────────────────────────

    fn check_ok_with_module(main_src: &str, module_name: &str, module_src: &str) {
        let main_prep = fai_compiler::prepare_source(main_src, None)
            .unwrap_or_else(|e| panic!("prepare main: {}", e));
        let mod_prep = fai_compiler::prepare_source(module_src, None)
            .unwrap_or_else(|e| panic!("prepare module: {}", e));
        let module = PreparedModule {
            name: module_name.to_string(),
            statements: mod_prep.serde_ast.statements,
            file_paths: Vec::new(),
            private_names: vec![],
            file_path: Some(format!("{}.fai", module_name)),
        };
        let mut checker = Checker::new();
        checker
            .check_with_modules(&main_prep.serde_ast.statements, &[module])
            .unwrap_or_else(|e| panic!("unexpected error: {}", e.message));
    }

    fn check_err_with_module(main_src: &str, module_name: &str, module_src: &str, expected: &str) {
        let main_prep = fai_compiler::prepare_source(main_src, None)
            .unwrap_or_else(|e| panic!("prepare main: {}", e));
        let mod_prep = fai_compiler::prepare_source(module_src, None)
            .unwrap_or_else(|e| panic!("prepare module: {}", e));
        let module = PreparedModule {
            name: module_name.to_string(),
            statements: mod_prep.serde_ast.statements,
            file_paths: Vec::new(),
            private_names: vec![],
            file_path: Some(format!("{}.fai", module_name)),
        };
        let mut checker = Checker::new();
        let err = checker
            .check_with_modules(&main_prep.serde_ast.statements, &[module])
            .expect_err(&format!(
                "expected error containing '{}' but check passed",
                expected
            ));
        assert!(
            err.message
                .to_lowercase()
                .contains(&expected.to_lowercase()),
            "expected '{}', got: '{}'",
            expected,
            err.message
        );
    }

    // ── Multi-module (check_with_modules) ─────────────────────────────

    #[test]
    fn test_module_cycle_resolves_cross_references() {
        // Regression for the partners project: two modules that import
        // from each other used to fail with cascades of `Unknown name`
        // errors because `module_type_exports` was populated
        // incrementally, one module at a time. The fix seeds every
        // module's export table from its declarations before any body
        // checking. This test mirrors the shape of the partners bug —
        // `server` holds the types, `server.parse` holds helpers that
        // parse those types and is imported back by `server`.
        //
        // Both modules should type-check even though they reference
        // each other's names.
        let server_src = "\
            use { parseTeam } from server.parse\n\
            type Team\n  id Int\n  name String\nend\n\n\
            # List teams.\n\
            def listTeams\n    @param raw String\n    @return Team\n\
            do\n  parseTeam(raw)\nend\n";
        let parse_src = "\
            use { Team } from server\n\n\
            # Parse a Team from raw text.\n\
            def parseTeam\n    @param raw String\n    @return Team\n\
            do\n  Team(id: 1, name: raw)\nend\n";
        let server_prep = fai_compiler::prepare_source(server_src, None)
            .unwrap_or_else(|e| panic!("prepare server: {}", e));
        let parse_prep = fai_compiler::prepare_source(parse_src, None)
            .unwrap_or_else(|e| panic!("prepare parse: {}", e));
        let modules = vec![
            PreparedModule {
                name: "server".to_string(),
                statements: server_prep.serde_ast.statements,
                file_paths: Vec::new(),
                private_names: vec![],
                file_path: Some("server.fai".to_string()),
            },
            PreparedModule {
                name: "server.parse".to_string(),
                statements: parse_prep.serde_ast.statements,
                file_paths: Vec::new(),
                private_names: vec![],
                file_path: Some("parse.fai".to_string()),
            },
        ];
        // Entry is empty — only the two cyclic modules matter here.
        let entry_prep = fai_compiler::prepare_source(
            "def main\n    @return Void\ndo\n  print('ok')\nend",
            None,
        )
        .unwrap();
        let mut checker = Checker::new();
        checker
            .check_with_modules(&entry_prep.serde_ast.statements, &modules)
            .unwrap_or_else(|e| {
                panic!(
                    "cyclic imports between 'server' and 'server.parse' should type-check. \
                 All collected errors:\n{}",
                    checker
                        .collected_errors
                        .iter()
                        .map(|e| e.message.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            });
    }

    #[test]
    fn test_module_import_function_ok() {
        check_ok_with_module(
            "use { greet } from mymod\n\ndef main\n    @return String\ndo\n  greet('world')\nend",
            "mymod",
            "# Greet.\ndef greet\n    @param name String\n    @return String\ndo\n  'hello ' + name\nend",
        );
    }

    #[test]
    fn test_module_namespace_import_ok() {
        check_ok_with_module(
            "use mymod\n\ndef main\n    @return String\ndo\n  mymod.greet('world')\nend",
            "mymod",
            "# Greet.\ndef greet\n    @param name String\n    @return String\ndo\n  'hello ' + name\nend",
        );
    }

    #[test]
    fn test_module_glob_import_ok() {
        check_ok_with_module(
            "use * from mymod\n\ndef main\n    @return String\ndo\n  greet('world')\nend",
            "mymod",
            "# Greet.\ndef greet\n    @param name String\n    @return String\ndo\n  'hello ' + name\nend",
        );
    }

    #[test]
    fn test_module_glob_import_collision_errors() {
        let entry = fai_compiler::prepare_source(
            "use { greet } from mymod\nuse * from other\n\ndef main\n    @return Void\ndo\nend",
            None,
        )
        .expect("prepare entry");
        let mymod = fai_compiler::prepare_source(
            "# Greet.\ndef greet\n    @return String\ndo\n  'hello'\nend",
            None,
        )
        .expect("prepare mymod");
        let other = fai_compiler::prepare_source(
            "# Greet.\ndef greet\n    @return Int\ndo\n  1\nend",
            None,
        )
        .expect("prepare other");
        let modules = vec![
            PreparedModule {
                name: "mymod".to_string(),
                statements: mymod.serde_ast.statements,
                file_paths: Vec::new(),
                private_names: vec![],
                file_path: Some("mymod.fai".to_string()),
            },
            PreparedModule {
                name: "other".to_string(),
                statements: other.serde_ast.statements,
                file_paths: Vec::new(),
                private_names: vec![],
                file_path: Some("other.fai".to_string()),
            },
        ];
        let mut checker = Checker::new();
        let err = checker
            .check_with_modules(&entry.serde_ast.statements, &modules)
            .expect_err("glob import should reject incompatible duplicate names");
        assert!(
            err.message.contains("already in scope"),
            "expected collision message, got: {}",
            err.message
        );
    }

    #[test]
    fn test_module_glob_import_duplicate_same_type_ok() {
        check_ok_with_module(
            "use { greet } from mymod\nuse * from mymod\n\ndef main\n    @return String\ndo\n  greet('world')\nend",
            "mymod",
            "# Greet.\ndef greet\n    @param name String\n    @return String\ndo\n  'hello ' + name\nend",
        );
    }

    #[test]
    fn test_module_import_missing_export_errors() {
        check_err_with_module(
            "use { nonexistent } from mymod\n\ndef main\n    @return Void\ndo\nend",
            "mymod",
            "# Greet.\ndef greet\n    @param name String\n    @return String\ndo\n  'hello ' + name\nend",
            "does not export",
        );
    }

    #[test]
    fn test_missing_required_argument_includes_signature() {
        // Regression test: when a call is missing a required arg,
        // the error today says "Missing required argument 'X' for
        // 'F'" with no hint of the call's full signature. Agents
        // hit this on Forsqlite (`query_params(db, sql)` without
        // the `params` array) and have to make a separate
        // `fai doc` round-trip to find the shape. Inline the
        // signature so they fix in one turn.
        let mut checker = crate::Checker::new();
        let prep = fai_compiler::prepare_source(
            concat!(
                "# Stub external API.\n",
                "def fetchPost\n",
                "    @param id Int\n",
                "    @param includeBody Bool\n",
                "    @return Int\n",
                "do\n",
                "  id\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  fetchPost(id: 42)\n",
                "end\n",
            ),
            None,
        )
        .unwrap();
        let err = checker
            .check_program(&prep.serde_ast.statements)
            .expect_err("missing arg should fail");
        assert!(
            err.message.contains("includeBody"),
            "error should name the missing parameter, got:\n{}",
            err.message
        );
        // The new bit: the full signature should be inlined so the
        // agent sees both required args and their types.
        assert!(
            err.message.contains("id: Int"),
            "error should inline the signature with `id: Int`, got:\n{}",
            err.message
        );
        assert!(
            err.message.contains("includeBody: Bool"),
            "error should inline `includeBody: Bool` so agent sees what's missing, got:\n{}",
            err.message
        );
        // Reference to `fai doc` for the canonical full signature
        // (mirrors the pattern used by other improved messages).
        assert!(
            err.message.contains("fai doc fetchPost"),
            "error should point at `fai doc fetchPost` for the canonical signature, got:\n{}",
            err.message
        );
    }

    #[test]
    fn test_useSignal_loader_mismatch_shows_typed_default_hint() {
        // Regression test: `useSignal(null) do getPost() end` — agent
        // passes null/empty as initial and a real-typed loader, fai
        // infers signal element as `null?` and the loader return
        // doesn't match. The error must steer to "pass a typed
        // default" rather than letting the agent waste turns trying
        // to make the loader return null.
        //
        // We use a minimal stub of useSignal here so the test doesn't
        // depend on the forui dependency tree being available.
        let mut checker = crate::Checker::new();
        let prep = fai_compiler::prepare_source(
            concat!(
                "type Post\n  id Int\n  title String\nend\n\n",
                "# Stub useSignal taking a loader.\n",
                "def useSignal\n",
                "    @type T\n",
                "    @param initial $T\n",
                "    @param loader () -> $T?\n",
                "    @return Int\ndo\n  0\nend\n\n",
                "# Stub.\ndef getPost\n    @return Post\ndo\n  Post(id: 1, title: 'x')\nend\n\n",
                "def main\n    @return Int\ndo\n  useSignal(null, loader: do getPost() end)\nend",
            ),
            None,
        )
        .unwrap();
        let err = checker
            .check_program(&prep.serde_ast.statements)
            .expect_err("useSignal(null, ...) should fail when loader returns Post");
        assert!(
            err.message.contains("typed default") || err.message.contains("typed initial"),
            "error should mention passing a typed default, got:\n{}",
            err.message
        );
        assert!(
            err.message.contains("useSignal(defaultPost)")
                || err.message.contains("useSignal(default"),
            "error should show the canonical fix snippet, got:\n{}",
            err.message
        );
    }

    #[test]
    fn test_module_import_missing_export_hints_fai_doc() {
        // Regression test: when `Module M does not export X` fires,
        // the agent has no idea where X actually lives or whether it
        // exists at all. The error should point at `fai doc <name>`
        // — agents already use `fai doc` proactively, so the hint
        // matches their existing instinct.
        check_err_with_module(
            "use { nonexistent } from mymod\n\ndef main\n    @return Void\ndo\nend",
            "mymod",
            "# Greet.\ndef greet\n    @param name String\n    @return String\ndo\n  'hello ' + name\nend",
            "fai doc nonexistent",
        );
    }

    #[test]
    fn test_std_module_missing_export_hints_fai_doc() {
        // Same hint applies to std modules — agents commonly invent
        // names like `std.string.toLowerCase` (from JS) when forai
        // exports `toLower`. The error should send them to `fai doc`.
        check_err(
            "use { toLowerCase } from std.string\n\ndef main\n    @return Void\ndo\nend",
            "fai doc toLowerCase",
        );
    }

    #[test]
    fn test_module_error_has_file_location() {
        // Errors from check_with_modules should have file location (covers error.rs with_location)
        let main_prep = fai_compiler::prepare_source(
            "use mymod\n\ndef main\n    @return Int\ndo\n  mymod.greet(42)\nend",
            None,
        )
        .unwrap();
        let mod_prep = fai_compiler::prepare_source(
            "# Greet.\ndef greet\n    @param name String\n    @return String\ndo\n  name\nend",
            None,
        )
        .unwrap();
        let module = PreparedModule {
            name: "mymod".to_string(),
            statements: mod_prep.serde_ast.statements,
            file_paths: Vec::new(),
            private_names: vec![],
            file_path: Some("mymod.fai".to_string()),
        };
        let mut checker = Checker::new();
        let err = checker
            .check_with_modules(&main_prep.serde_ast.statements, &[module])
            .expect_err("expected type error");
        // Error should have location attached
        assert!(!err.message.is_empty());
    }

    // ── Extern block with opaque type ──────────────────────────────────

    #[test]
    fn test_extern_block_with_opaque_type_ok() {
        check_ok(concat!(
            "extern libc\n",
            "  type FILE\n",
            "  def fopen(path: String) -> FILE\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print('ok')\n",
            "end\n",
        ));
    }

    // ── Top-level assignment ───────────────────────────────────────────

    #[test]
    fn test_top_level_assignment_ok() {
        check_ok("var count = 0\ncount = 5\n\ndef main\n    @return Int\ndo\n  count\nend");
    }

    // ── Multi-return function type mismatches ──────────────────────────

    #[test]
    fn test_multi_return_wrong_element_type_errors() {
        // Returns (String, Int) but declares (@return Int, @return String)
        check_err(
            concat!(
                "# Bad multi-return.\n",
                "def bad\n",
                "    @return Int\n",
                "    @return String\n",
                "do\n",
                "  'hello', 42\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "end\n",
            ),
            "return value",
        );
    }

    #[test]
    fn test_multi_return_non_tuple_body_errors() {
        // Body returns single value when multi-return declared
        check_err(
            concat!(
                "# Non-tuple body.\n",
                "def bad\n",
                "    @return Int\n",
                "    @return String\n",
                "do\n",
                "  42\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "end\n",
            ),
            "must return",
        );
    }

    // ── Multiple let/var bindings edge cases ──────────────────────────

    #[test]
    fn test_multiple_let_binding_requires_tuple_errors() {
        // let a, b = 5  — 5 is not a tuple
        check_err(
            "def main\n    @return Void\ndo\n  let a, b = 5\nend",
            "multiple",
        );
    }

    #[test]
    fn test_multiple_let_binding_with_declared_type_mismatch_errors() {
        // let a, b String = 1, 2  — 2 is Int but b expects String
        check_err(
            "def main\n    @return Void\ndo\n  let a Int, b String = 1, 2\nend",
            "Cannot assign",
        );
    }

    // ── Use statement inside function body ─────────────────────────────

    #[test]
    fn test_use_inside_function_ok() {
        // use inside a function body is syntactically valid — checker returns Void for it
        check_ok(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  use std.math\n",
            "end\n",
        ));
    }

    // ── Local function definition inside a block ───────────────────────

    #[test]
    fn test_local_function_inside_block_ok() {
        check_ok(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  # A local helper.\n",
            "  def double\n",
            "      @param x Int\n",
            "      @return Int\n",
            "  do\n",
            "    x * 2\n",
            "  end\n",
            "  double(5)\n",
            "end\n",
        ));
    }

    // ── Test declaration via checker ───────────────────────────────────

    #[test]
    fn test_check_test_declaration_ok() {
        // test greet — greet is a function, so check_test_declaration succeeds
        check_ok(concat!(
            "use std.test\n",
            "\n",
            "# A greeting function.\n",
            "def greet\n",
            "    @param name String\n",
            "    @return String\n",
            "do\n",
            "  'hello ' + name\n",
            "end\n",
            "\n",
            "test greet\n",
            "  it 'returns hello world'\n",
            "    test.equal(greet('world'), 'hello world')\n",
            "  end\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "end\n",
        ));
    }

    #[test]
    fn test_check_test_declaration_non_function_errors() {
        // test x — but x is an Int, not a function
        check_err(
            "let x = 42\n\ntest x\n  it 'case'\n  end\nend\n\ndef main\n    @return Void\ndo\nend",
            "must refer to a function",
        );
    }

    // ── Binary arithmetic type results ─────────────────────────────────

    #[test]
    fn test_add_int_and_float_returns_float() {
        check_ok("def main\n    @return Float\ndo\n  1 + 2.0\nend");
    }

    #[test]
    fn test_add_float_and_float_returns_float() {
        check_ok("def main\n    @return Float\ndo\n  1.5 + 2.5\nend");
    }

    #[test]
    fn test_subtract_mixed_returns_float() {
        check_ok("def main\n    @return Float\ndo\n  3.0 - 1\nend");
    }

    #[test]
    fn test_divide_non_numeric_errors() {
        check_err(
            "def main\n    @return Void\ndo\n  let x = 'a' / 1\nend",
            "requires numeric",
        );
    }

    #[test]
    fn test_floor_div_int_int_returns_int() {
        check_ok("def main\n    @return Int\ndo\n  5 // 2\nend");
    }

    #[test]
    fn test_floor_div_mixed_returns_float() {
        check_ok("def main\n    @return Float\ndo\n  5.0 // 2\nend");
    }

    #[test]
    fn test_pow_int_int_returns_int() {
        check_ok("def main\n    @return Int\ndo\n  2 ** 10\nend");
    }

    #[test]
    fn test_pow_mixed_returns_float() {
        check_ok("def main\n    @return Float\ndo\n  2.0 ** 3\nend");
    }

    // ── Null comparison (== and != with optional) ──────────────────────

    #[test]
    fn test_optional_compare_null_ok() {
        check_ok("def main\n    @return Bool\ndo\n  let x Int? = null\n  x == null\nend");
    }

    // ── Tuple binding with Unknown type (from all()) ────────────────────

    #[test]
    fn test_multiple_let_from_all_ok() {
        check_ok(concat!(
            "# Task.\n",
            "def task\n",
            "    @return Int\n",
            "do\n",
            "  1\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let a, b = all(task(), task())\n",
            "end\n",
        ));
    }

    // ── Ensure consistent branch types (Never branches) ─────────────────

    #[test]
    fn test_if_with_throw_in_one_branch_ok() {
        // throw returns Never, which is compatible with any type
        check_ok(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  if true\n",
            "    throw 'error'\n",
            "  else\n",
            "    42\n",
            "  end\n",
            "end\n",
        ));
    }

    // ── Generic return-type resolution for array.slice / array.reverse ─

    // Regression: arraySlice and arrayReverse have signature T[] -> T[],
    // but accept_builtin_special_case bypasses bind_and_check_assignable,
    // leaving generic_bindings empty so the return type stays as $T[].
    // Assigning the result to a typed variable then fails. See
    // plans/bug-optional-typedef-default-null.md for context (the second
    // bug listed there).

    #[test]
    fn test_array_slice_preserves_element_type_int() {
        check_ok(concat!(
            "use std.array\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  var nums Int[] = [1 2 3 4 5]\n",
            "  nums = array.slice(nums, 1, 3)\n",
            "  print(nums[0])\n",
            "end\n",
        ));
    }

    #[test]
    fn test_array_slice_preserves_element_type_string() {
        check_ok(concat!(
            "use std.array\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  var words String[] = ['a' 'b' 'c']\n",
            "  words = array.slice(words, 0, 2)\n",
            "  print(words[0])\n",
            "end\n",
        ));
    }

    #[test]
    fn test_array_slice_let_with_typed_binding() {
        check_ok(concat!(
            "use std.array\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let src Int[] = [10 20 30]\n",
            "  let out Int[] = array.slice(src, 0, 2)\n",
            "  print(out[0])\n",
            "end\n",
        ));
    }

    #[test]
    fn test_array_reverse_preserves_element_type() {
        check_ok(concat!(
            "use std.array\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  var nums Int[] = [1 2 3]\n",
            "  nums = array.reverse(nums)\n",
            "  print(nums[0])\n",
            "end\n",
        ));
    }

    // ── Multiple modules and cycles ────────────────────────────────────

    #[test]
    fn test_two_modules_ok() {
        let main_prep = fai_compiler::prepare_source(
            "use mod_a\nuse mod_b\n\ndef main\n    @return Void\ndo\n  mod_a.hello()\n  mod_b.world()\nend",
            None,
        ).unwrap();
        let mod_a =
            fai_compiler::prepare_source("# Hello.\ndef hello\n    @return Void\ndo\nend", None)
                .unwrap();
        let mod_b =
            fai_compiler::prepare_source("# World.\ndef world\n    @return Void\ndo\nend", None)
                .unwrap();
        let modules = vec![
            PreparedModule {
                name: "mod_a".to_string(),
                statements: mod_a.serde_ast.statements,
                file_paths: Vec::new(),
                private_names: vec![],
                file_path: None,
            },
            PreparedModule {
                name: "mod_b".to_string(),
                statements: mod_b.serde_ast.statements,
                file_paths: Vec::new(),
                private_names: vec![],
                file_path: None,
            },
        ];
        let mut checker = Checker::new();
        checker
            .check_with_modules(&main_prep.serde_ast.statements, &modules)
            .unwrap();
    }

    // ── Generic inference from function argument return types ──────

    #[test]
    fn test_generic_inferred_from_loader_with_default_null() {
        // This is the exact pattern from useSignal: optional loader param
        // where the default value [] should be refined by the loader's return type.
        check_ok(
            r#"
type def Loader
    @return $T
end

# Create signal.
def useSignal
    @type T
    @param initialValue $T
    @param loader Loader?, default: null
    @return $T
do
  initialValue
end

type Task
  id Int
end

# Get tasks.
def getTasks
    @return Task[]
do
  let empty Task[] = []
  empty
end

def main
    @return Void
do
  let tasks = useSignal([], getTasks)
end
"#,
        );
    }

    #[test]
    fn test_generic_inferred_from_loader_return_type() {
        // A generic function that takes a loader function should infer
        // the type parameter from the loader's return type.
        check_ok(
            r#"
type def Loader
    @return $T
end

type Box
  value $T
  loader Loader?
end

# Create box.
def createBox
    @type T
    @param defaultValue $T
    @param loader Loader?, default: null
    @return Box
do
  Box(value: defaultValue, loader: loader)
end

type Task
  id Int
  text String
end

# Get tasks.
def getTasks
    @return Task[]
do
  let empty Task[] = []
  empty
end

def main
    @return Void
do
  let b = createBox([], getTasks)
end
"#,
        );
    }

    #[test]
    fn test_generic_inferred_from_closure_loader() {
        // Same but with an inline closure as the loader
        check_ok(
            r#"
type def Loader
    @return $T
end

# Create.
def make
    @type T
    @param defaultValue $T
    @param loader Loader?, default: null
    @return $T
do
  defaultValue
end

def main
    @return Void
do
  let x = make(0, do 42 end)
end
"#,
        );
    }
}
