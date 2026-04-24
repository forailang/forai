pub mod ast;
pub mod lexer;
pub mod parser;
pub mod token;

use ast::Program;
use lexer::Lexer;
use parser::Parser;

/// Parse FAI source code into an AST.
pub fn parse(source: &str) -> Result<Program, String> {
    let tokens = Lexer::new(source).scan_tokens()?;
    let mut parser = Parser::new(tokens);
    parser.parse_program()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ast::Statement;

    fn parse_ok(source: &str) -> ast::Program {
        parse(source).unwrap_or_else(|e| panic!("parse error: {}", e))
    }

    #[test]
    fn test_parse_let() {
        let p = parse_ok("let x = 42");
        assert_eq!(p.statements.len(), 1);
        assert!(matches!(&p.statements[0], Statement::Let(_)));
    }

    #[test]
    fn test_parse_var() {
        let p = parse_ok("var count = 0");
        assert_eq!(p.statements.len(), 1);
        assert!(matches!(&p.statements[0], Statement::Var(_)));
    }

    #[test]
    fn test_parse_function() {
        let p = parse_ok("# Add numbers.\ndef add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n  a + b\nend");
        assert_eq!(p.statements.len(), 1);
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.name, "add");
            assert_eq!(f.params.len(), 2);
        } else {
            panic!("expected function declaration");
        }
    }

    #[test]
    fn test_parse_type_declaration() {
        let p = parse_ok("type Point\n  x Int\n  y Int\nend");
        assert_eq!(p.statements.len(), 1);
        if let Statement::Type(t) = &p.statements[0] {
            assert_eq!(t.name, "Point");
            assert_eq!(t.fields.len(), 2);
        } else {
            panic!("expected type declaration");
        }
    }

    #[test]
    fn test_parse_enum() {
        let p = parse_ok("enum Color\n  red\n  green\n  blue\nend");
        assert_eq!(p.statements.len(), 1);
        if let Statement::Enum(e) = &p.statements[0] {
            assert_eq!(e.name, "Color");
            assert_eq!(e.members.len(), 3);
        } else {
            panic!("expected enum declaration");
        }
    }

    #[test]
    fn test_parse_if_else() {
        let p = parse_ok("if x > 5\n  print(x)\nelse\n  print(0)\nend");
        assert_eq!(p.statements.len(), 1);
        assert!(matches!(&p.statements[0], Statement::If(_)));
    }

    #[test]
    fn test_parse_for_loop() {
        let p = parse_ok("for i in items\n  print(i)\nend");
        assert_eq!(p.statements.len(), 1);
        assert!(matches!(&p.statements[0], Statement::For(_)));
    }

    #[test]
    fn test_parse_do_block_no_params() {
        parse_ok("def main\n    @return String\ndo\n  let f = do\n    print('hello')\n  end\n  'done'\nend");
    }

    #[test]
    fn test_parse_do_block_with_params() {
        parse_ok("def main\n    @return String\ndo\n  let f = do with x Int\n    print(x)\n  end\n  'done'\nend");
    }

    #[test]
    fn test_parse_do_block_as_inline_arg() {
        parse_ok("use std.array\n\n# Check if even.\ndef isEven\n    @param n Int\n    @return Bool\ndo\n  n == 0\nend\n\ndef main\n    @return String\ndo\n  let items = [1 2 3]\n  array.filter(items, do with n Int\n    n == 2\n  end)\n  'done'\nend");
    }

    #[test]
    fn test_parse_do_block_trailing() {
        parse_ok("# Run function.\ndef run\n    @param fn (Int) -> Int\n    @return Int\ndo\n  fn(5)\nend\n\ndef main\n    @return Int\ndo\n  run() do with n Int\n    n * 2\n  end\nend");
    }

    #[test]
    fn test_parse_function_typed_parameter_before_plain_parameter() {
        parse_ok("# Mount app.\ndef mount\n    @param app () -> String\n    @param selector String\n    @return String\ndo\n  app()\nend");
    }

    #[test]
    fn test_parse_function_typed_parameter_between_plain_parameters() {
        parse_ok("# Wrap string.\ndef wrap\n    @param prefix String\n    @param transform (String) -> String\n    @param suffix String\n    @return String\ndo\n  suffix\nend");
    }

    #[test]
    fn test_parse_while_loop() {
        let p = parse_ok("while x < 10\n  x = x + 1\nend");
        assert_eq!(p.statements.len(), 1);
        assert!(matches!(&p.statements[0], Statement::While(_)));
    }

    #[test]
    fn test_parse_try_catch() {
        let p = parse_ok("try\n  risky()\ncatch err\n  print(err)\nend");
        assert_eq!(p.statements.len(), 1);
        assert!(matches!(&p.statements[0], Statement::Try(_)));
    }

    #[test]
    fn test_parse_use_statement() {
        let p = parse_ok("use std.io");
        assert_eq!(p.statements.len(), 1);
        if let Statement::Use(u) = &p.statements[0] {
            assert_eq!(u.module_path, vec!["std", "io"]);
        } else {
            panic!("expected use statement");
        }
    }

    #[test]
    fn test_parse_use_from() {
        let p = parse_ok("use { map, filter } from std.array");
        assert_eq!(p.statements.len(), 1);
        if let Statement::Use(u) = &p.statements[0] {
            assert_eq!(u.module_path, vec!["std", "array"]);
            assert_eq!(u.imported_names.as_ref().unwrap(), &vec!["map", "filter"]);
        } else {
            panic!("expected use statement");
        }
    }

    #[test]
    fn test_parse_case() {
        let p = parse_ok("case x\nwhen 1\n  print('one')\ndefault\n  print('other')\nend");
        assert_eq!(p.statements.len(), 1);
        assert!(matches!(&p.statements[0], Statement::Case(_)));
    }

    #[test]
    fn test_parse_break_continue() {
        let p = parse_ok("for i in items\n  break\nend");
        if let Statement::For(f) = &p.statements[0] {
            assert!(matches!(&f.body[0], Statement::Break(_)));
        } else {
            panic!("expected for statement");
        }
    }

    #[test]
    fn test_parse_error_missing_end() {
        let result = parse("# Foo.\ndef foo\n    @return Int\ndo\n  42");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_multiple_statements() {
        let p = parse_ok("let x = 1\nlet y = 2\nlet z = 3");
        assert_eq!(p.statements.len(), 3);
    }

    #[test]
    fn test_parse_scientific_notation_number_literal() {
        let expr = Lexer::new("1.5e2")
            .scan_tokens()
            .expect("lex should succeed");
        let mut parser = Parser::new(expr);
        let expr = parser
            .parse_standalone_expression()
            .expect("parse should succeed");
        if let ast::Expression::Number(n) = expr {
            assert_eq!(n.value, 150.0);
            assert!(n.is_float);
        } else {
            panic!("expected number expression");
        }
    }

    #[test]
    fn test_parse_scientific_notation_integer_mantissa_is_float() {
        let expr = Lexer::new("1e3").scan_tokens().expect("lex should succeed");
        let mut parser = Parser::new(expr);
        let expr = parser
            .parse_standalone_expression()
            .expect("parse should succeed");
        if let ast::Expression::Number(n) = expr {
            assert_eq!(n.value, 1000.0);
            assert!(n.is_float);
        } else {
            panic!("expected number expression");
        }
    }

    #[test]
    fn test_lexer_rejects_missing_scientific_exponent_digits() {
        let err = Lexer::new("1e").scan_tokens().expect_err("lex should fail");
        assert!(err.contains("Malformed scientific notation"));
    }

    #[test]
    fn test_lexer_rejects_scientific_exponent_sign_without_digits() {
        let err = Lexer::new("1e+").scan_tokens().expect_err("lex should fail");
        assert!(err.contains("Malformed scientific notation"));
    }

    #[test]
    fn test_lexer_rejects_multiple_decimal_segments() {
        let err = Lexer::new("1.2.3e4")
            .scan_tokens()
            .expect_err("lex should fail");
        assert!(err.contains("Malformed number literal"));
    }

    #[test]
    fn test_parse_inline_array_preserves_inline_style() {
        let expr = parse_ok("let xs = [1 2 3]");
        let Statement::Let(stmt) = &expr.statements[0] else {
            panic!("expected let statement");
        };
        let ast::Expression::Array(arr) = &stmt.value else {
            panic!("expected array expression");
        };
        assert!(matches!(arr.style, ast::ArrayLiteralStyle::Inline));
    }

    #[test]
    fn test_parse_vertical_array_preserves_vertical_style() {
        let expr = parse_ok("let xs = [\n  1\n  2\n  3\n]");
        let Statement::Let(stmt) = &expr.statements[0] else {
            panic!("expected let statement");
        };
        let ast::Expression::Array(arr) = &stmt.value else {
            panic!("expected array expression");
        };
        assert!(matches!(arr.style, ast::ArrayLiteralStyle::Vertical));
    }

    // ── New def syntax (v2) parser tests ────────────────────────

    #[test]
    fn test_parse_v2_basic() {
        let p = parse_ok("# Add numbers.\ndef add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n    a + b\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.name, "add");
            assert_eq!(f.params.len(), 2);
            assert_eq!(f.params[0].name, "a");
            assert_eq!(f.params[1].name, "b");
            assert_eq!(f.return_types.len(), 1);
            assert_eq!(f.doc_comment.as_deref(), Some("Add numbers."));
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_no_params() {
        let p = parse_ok("def main\n    @return Void\ndo\n    print('hi')\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.name, "main");
            assert_eq!(f.params.len(), 0);
            assert_eq!(f.return_types[0].type_node.name.as_deref(), Some("Void"));
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_defaults() {
        let p = parse_ok("# Connect.\ndef connect\n    @param host String, default: 'localhost'\n    @param port Int, default: 5432\n    @return Void\ndo\n    print(host)\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.params.len(), 2);
            assert!(f.params[0].default_value.is_some());
            assert!(f.params[1].default_value.is_some());
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_multiple_returns() {
        let p = parse_ok("# Swap values.\ndef swap\n    @param a Int\n    @param b Int\n    @return first Int\n    @return second Int\ndo\n    b, a\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.return_types.len(), 2);
            assert_eq!(f.return_types[0].name.as_deref(), Some("first"));
            assert_eq!(f.return_types[1].name.as_deref(), Some("second"));
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_generics() {
        let p = parse_ok("# Echo value.\ndef echo\n    @type T\n    @param value T\n    @return T\ndo\n    value\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.type_params.len(), 1);
            assert_eq!(f.type_params[0].name, "T");
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_multi_type_params() {
        let p = parse_ok("# Make pair.\ndef makePair\n    @type K\n    @type V\n    @param key K\n    @param value V\n    @return Void\ndo\n    print(key)\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.type_params.len(), 2);
            assert_eq!(f.type_params[0].name, "K");
            assert_eq!(f.type_params[1].name, "V");
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_doc_comments_on_params() {
        let p = parse_ok("# A function.\ndef foo\n    # The first param\n    @param a Int\n    # The second param\n    @param b Int\n    @return Int\ndo\n    a\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.params[0].doc_comment.as_deref(), Some("The first param"));
            assert_eq!(f.params[1].doc_comment.as_deref(), Some("The second param"));
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_multiline_doc() {
        let p = parse_ok(
            "# Line one.\n# Line two.\ndef foo\n    @param a Int\n    @return Int\ndo\n    a\nend",
        );
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.doc_comment.as_deref(), Some("Line one.\nLine two."));
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_doc_comment_preserves_indentation_after_hash() {
        // The lexer used to strip all whitespace after `#`, so a line
        // like `#     nested code` lost its indentation — catastrophic
        // for fenced code blocks in doc comments (examples rendered as
        // a flat wall of text). The current behaviour strips exactly
        // one leading space/tab; anything beyond that is preserved.
        let p = parse_ok(
            "# Example:\n# ```fai\n# VStack do\n#     Label('hi')\n# end\n# ```\ndef foo\n    @return Int\ndo\n    1\nend",
        );
        if let Statement::Function(f) = &p.statements[0] {
            let doc = f.doc_comment.as_deref().expect("doc comment");
            assert!(
                doc.contains("    Label('hi')"),
                "four-space indent inside fence should survive lexing, got:\n{}",
                doc
            );
            assert!(doc.contains("```fai"), "fenced code block should be intact");
            assert!(
                doc.contains("VStack do"),
                "un-indented line still un-indented"
            );
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_callback_param() {
        let p = parse_ok("# Apply function.\ndef apply\n    @param callback (Int) -> Int\n    @return Int\ndo\n    callback(5)\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert!(f.params[0].type_node.function_params.is_some());
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_void_return() {
        let p = parse_ok("# Greet.\ndef greet\n    @param name String\n    @return Void\ndo\n    print(name)\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.return_types[0].name, None);
            assert_eq!(f.return_types[0].type_node.name.as_deref(), Some("Void"));
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_v2_error_missing_do() {
        let result = parse("# Bad.\ndef foo\n    @param a Int\n    @return Int\n    a + 1\nend");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_v2_error_type_after_param() {
        let result = parse(
            "# Bad.\ndef foo\n    @param a Int\n    @type T\n    @return Int\ndo\n    a\nend",
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("@type must come before @param"));
    }

    #[test]
    fn test_parse_v2_error_param_after_return() {
        let result = parse("# Bad.\ndef foo\n    @return Int\n    @param a Int\ndo\n    a\nend");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("@param must come before @return"));
    }

    // ── type def parsing ────────────────────────────────────────

    #[test]
    fn test_parse_type_def_basic() {
        let p = parse_ok("type def Callback\n    @param n Int\n    @return Int\nend");
        assert_eq!(p.statements.len(), 1);
        if let Statement::FunctionTypeDef(ftd) = &p.statements[0] {
            assert_eq!(ftd.name, "Callback");
            assert_eq!(ftd.params.len(), 1);
            assert_eq!(ftd.params[0].name, "n");
            assert_eq!(ftd.return_types.len(), 1);
        } else {
            panic!("expected FunctionTypeDef");
        }
    }

    #[test]
    fn test_parse_type_def_no_params() {
        let p = parse_ok("type def Action\n    @return Void\nend");
        if let Statement::FunctionTypeDef(ftd) = &p.statements[0] {
            assert_eq!(ftd.name, "Action");
            assert_eq!(ftd.params.len(), 0);
        } else {
            panic!("expected FunctionTypeDef");
        }
    }

    #[test]
    fn test_parse_type_def_multiple_params() {
        let p = parse_ok("type def Reducer\n    @param state Int\n    @param action String\n    @return Int\nend");
        if let Statement::FunctionTypeDef(ftd) = &p.statements[0] {
            assert_eq!(ftd.name, "Reducer");
            assert_eq!(ftd.params.len(), 2);
            assert_eq!(ftd.params[0].name, "state");
            assert_eq!(ftd.params[1].name, "action");
        } else {
            panic!("expected FunctionTypeDef");
        }
    }

    // ── trailing do...end ───────────────────────────────────────

    #[test]
    fn test_parse_trailing_do_no_parens() {
        // `run do ... end` should parse as run(do...end)
        let p = parse_ok("# Init.\ndef run\n    @param f () -> Void\n    @return Void\ndo\n  f()\nend\n\ndef main\n    @return Void\ndo\n  run do\n    print('hi')\n  end\nend");
        // Should have 2 statements (run, main)
        assert_eq!(p.statements.len(), 2);
    }

    #[test]
    fn test_parse_trailing_do_after_parens() {
        // `apply(5) do ... end` should parse as apply(5, do...end)
        let p = parse_ok("# Apply.\ndef apply\n    @param n Int\n    @param f (Int) -> Int\n    @return Int\ndo\n  f(n)\nend\n\ndef main\n    @return Void\ndo\n  apply(5) do with n Int\n    n * 2\n  end\nend");
        assert_eq!(p.statements.len(), 2);
    }

    #[test]
    fn test_parse_trailing_do_with_member() {
        // `array.filter(items) do ... end` should parse
        parse_ok("use std.array\n\ndef main\n    @return Void\ndo\n  let items = [1 2 3]\n  array.filter(items) do with n Int\n    n == 2\n  end\nend");
    }

    // ── Plan 101 Phase 2: Abstract function declarations ──────

    #[test]
    fn test_parse_abstract_function() {
        // `def` without `do...end` is an abstract declaration (interface)
        let p = parse_ok("# Get all tasks.\ndef getTasks\n    @return Int[]");
        assert_eq!(p.statements.len(), 1);
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.name, "getTasks");
            assert!(f.body.is_empty());
            assert!(f.is_abstract);
        } else {
            panic!("expected function declaration");
        }
    }

    #[test]
    fn test_parse_abstract_function_with_params() {
        let p = parse_ok("# Add a task.\ndef addTask\n    @param text String\n    @return Int");
        assert_eq!(p.statements.len(), 1);
        if let Statement::Function(f) = &p.statements[0] {
            assert_eq!(f.name, "addTask");
            assert_eq!(f.params.len(), 1);
            assert_eq!(f.params[0].name, "text");
            assert!(f.body.is_empty());
            assert!(f.is_abstract);
        } else {
            panic!("expected function declaration");
        }
    }

    #[test]
    fn test_parse_concrete_function_not_abstract() {
        let p = parse_ok("# Add.\ndef add\n    @param a Int\n    @param b Int\n    @return Int\ndo\n  a + b\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert!(!f.is_abstract);
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_abstract_followed_by_concrete() {
        // Both abstract and concrete functions in the same file
        let p = parse_ok(
            "# Get.\ndef getTasks\n    @return Int[]\n\n# Main.\ndef main\n    @return Void\ndo\n  print('hi')\nend"
        );
        assert_eq!(p.statements.len(), 2);
        if let Statement::Function(f) = &p.statements[0] {
            assert!(f.is_abstract);
            assert_eq!(f.name, "getTasks");
        } else {
            panic!("expected function");
        }
        if let Statement::Function(f) = &p.statements[1] {
            assert!(!f.is_abstract);
            assert_eq!(f.name, "main");
        } else {
            panic!("expected function");
        }
    }

    // ── remote def ────────────────────────────────────────────────

    #[test]
    fn test_parse_remote_def_abstract() {
        let p = parse_ok("# Get tasks.\nremote def getTasks\n    @return Task[]");
        if let Statement::Function(f) = &p.statements[0] {
            assert!(f.is_remote);
            assert!(f.is_abstract);
            assert_eq!(f.name, "getTasks");
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_remote_def_with_body() {
        let p = parse_ok("# Get tasks.\nremote def getTasks\n    @return Int\ndo\n  42\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert!(f.is_remote);
            assert!(!f.is_abstract);
            assert_eq!(f.name, "getTasks");
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_remote_def_with_params() {
        let p = parse_ok("# Add.\nremote def addTask\n    @param text String\n    @return Int");
        if let Statement::Function(f) = &p.statements[0] {
            assert!(f.is_remote);
            assert_eq!(f.params.len(), 1);
            assert_eq!(f.params[0].name, "text");
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_non_remote_def_is_not_remote() {
        let p = parse_ok("# Add.\ndef add\n    @param a Int\n    @return Int\ndo\n  a\nend");
        if let Statement::Function(f) = &p.statements[0] {
            assert!(!f.is_remote);
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn test_parse_mixed_remote_and_normal() {
        let p = parse_ok(concat!(
            "# Helper.\ndef helper\n    @return Int\ndo\n  1\nend\n\n",
            "# Remote.\nremote def getData\n    @return String\n\n",
            "# Main.\ndef main\n    @return Void\ndo\n  print('hi')\nend\n"
        ));
        assert_eq!(p.statements.len(), 3);
        if let Statement::Function(f) = &p.statements[0] {
            assert!(!f.is_remote);
        } else {
            panic!("expected function");
        }
        if let Statement::Function(f) = &p.statements[1] {
            assert!(f.is_remote);
            assert_eq!(f.name, "getData");
        } else {
            panic!("expected function");
        }
        if let Statement::Function(f) = &p.statements[2] {
            assert!(!f.is_remote);
        } else {
            panic!("expected function");
        }
    }

    // ── field attributes ─────────────────────────────────────────────

    #[test]
    fn test_field_no_attributes() {
        let p = parse_ok("type User\n  name String\nend");
        if let Statement::Type(td) = &p.statements[0] {
            assert_eq!(td.fields[0].attributes.len(), 0);
        } else {
            panic!("expected type");
        }
    }

    #[test]
    fn test_field_string_attribute() {
        let p = parse_ok("type User\n  userName String, alias: 'user_name'\nend");
        if let Statement::Type(td) = &p.statements[0] {
            let attrs = &td.fields[0].attributes;
            assert_eq!(attrs.len(), 1);
            assert_eq!(attrs[0].key, "alias");
            assert!(
                matches!(&attrs[0].value, ast::FieldAttributeValue::String(s) if s == "user_name")
            );
        } else {
            panic!("expected type");
        }
    }

    #[test]
    fn test_field_flag_attribute() {
        let p = parse_ok("type User\n  password String, omit\nend");
        if let Statement::Type(td) = &p.statements[0] {
            let attrs = &td.fields[0].attributes;
            assert_eq!(attrs.len(), 1);
            assert_eq!(attrs[0].key, "omit");
            assert!(matches!(&attrs[0].value, ast::FieldAttributeValue::Flag));
        } else {
            panic!("expected type");
        }
    }

    #[test]
    fn test_field_multiple_attributes() {
        let p = parse_ok("type User\n  userName String, alias: 'user_name', omit\nend");
        if let Statement::Type(td) = &p.statements[0] {
            let attrs = &td.fields[0].attributes;
            assert_eq!(attrs.len(), 2);
            assert_eq!(attrs[0].key, "alias");
            assert_eq!(attrs[1].key, "omit");
        } else {
            panic!("expected type");
        }
    }

    #[test]
    fn test_field_attribute_with_default_value() {
        // attributes and defaults can coexist
        let p = parse_ok("type Config\n  debug Bool = false, omit\nend");
        if let Statement::Type(td) = &p.statements[0] {
            assert!(td.fields[0].default_value.is_some());
            assert_eq!(td.fields[0].attributes[0].key, "omit");
        } else {
            panic!("expected type");
        }
    }

    #[test]
    fn test_field_custom_attribute() {
        // Unknown attribute keys pass through without error
        let p = parse_ok("type Product\n  price Float, db_column: 'product_price'\nend");
        if let Statement::Type(td) = &p.statements[0] {
            let attrs = &td.fields[0].attributes;
            assert_eq!(attrs[0].key, "db_column");
            assert!(
                matches!(&attrs[0].value, ast::FieldAttributeValue::String(s) if s == "product_price")
            );
        } else {
            panic!("expected type");
        }
    }

    #[test]
    fn test_field_attribute_non_string_value_is_error() {
        let result = parse("type User\n  name String, alias: 42\nend");
        assert!(result.is_err(), "non-string attribute value should fail");
    }

    // ── Boolean operators ────────────────────────────────────────────

    fn binary_op(p: &ast::Program) -> String {
        match &p.statements[0] {
            Statement::Let(l) => match &l.value {
                ast::Expression::Binary(b) => b.operator.clone(),
                other => panic!("expected Binary, got {:?}", other),
            },
            other => panic!("expected Let, got {:?}", other),
        }
    }

    fn unary_op(p: &ast::Program) -> String {
        match &p.statements[0] {
            Statement::Let(l) => match &l.value {
                ast::Expression::Unary(u) => u.operator.clone(),
                other => panic!("expected Unary, got {:?}", other),
            },
            other => panic!("expected Let, got {:?}", other),
        }
    }

    #[test]
    fn test_bool_and_keyword() {
        let p = parse_ok("let x = true and false");
        assert_eq!(binary_op(&p), "and");
    }

    #[test]
    fn test_bool_or_keyword() {
        let p = parse_ok("let x = true or false");
        assert_eq!(binary_op(&p), "or");
    }

    #[test]
    fn test_bool_not_keyword() {
        let p = parse_ok("let x = not true");
        assert_eq!(unary_op(&p), "not");
    }

    #[test]
    fn test_bool_bang_symbol() {
        let p = parse_ok("let x = !true");
        assert_eq!(unary_op(&p), "!");
    }

    #[test]
    fn test_ampamp_rejected() {
        // `&&` is no longer part of the language. The lexer reports
        // `&` as an unknown character and `parse` returns Err.
        let result = parse("let x = true && false");
        assert!(result.is_err(), "&& should not parse");
        let msg = result.unwrap_err();
        assert!(
            msg.contains('&'),
            "error should mention the bad char: {}",
            msg
        );
    }

    #[test]
    fn test_pipepipe_rejected() {
        let result = parse("let x = true || false");
        assert!(result.is_err(), "|| should not parse");
        let msg = result.unwrap_err();
        assert!(
            msg.contains('|'),
            "error should mention the bad char: {}",
            msg
        );
    }
}
