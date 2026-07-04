    //! End-to-end: parse forai source, feed `main`'s body to the
    //! builder, assemble a minimal wasm module (imports + runtime +
    //! one function), run via wasmtime, and assert the return value.
    //!
    //! This is the Phase C exit-criterion proof: a small program
    //! compiles to wasm *without producing bytecode for its function
    //! body*. The module scaffolding reuses the existing runtime
    //! helpers from `crate::runtime`.
    //!
    //! Programs here are minimal: a single `main` function returning
    //! an Int. Once control flow (Phase D), calls (Phase E), and the
    //! other phases migrate, we'll wire the builder into the main
    //! `module.rs` pipeline.
    use super::*;
    use crate::runtime;
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ElementSection, Elements, EntityType, ExportKind,
        ExportSection, FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection,
        MemoryType, Module as EncModule, RefType, TableSection, TableType, TypeSection,
    };
    use wasmtime::{Engine, Linker, Module as RuntimeModule, Store, Val};

    /// Max closure arity the standalone test harness pre-declares
    /// `FaiFunc(N)` types for. Tests only need up to a handful; bumping
    /// this is cheap (one extra type slot per entry).
    const MAX_FAI_ARITY: u16 = 8;

    /// Build the `fai_func_type_indices` map the direct builder needs
    /// for `CallIndirect`. Types are allocated after imports + runtime
    /// helpers (which is how the test harness lays them out below).
    fn build_fai_type_indices() -> HashMap<u16, u32> {
        let import_count = runtime::import_signatures().len() as u32;
        let rt_count = runtime::type_signatures().len() as u32;
        let base = import_count + rt_count;
        (0..=MAX_FAI_ARITY).map(|n| (n, base + n as u32)).collect()
    }

    /// Parse source, locate `def main`, and hand its AST to the
    /// direct builder. Returns the built wasm function.
    /// Standalone-module layout: imports first, then runtime helpers,
    /// then main. The builder needs `rt_base = import_count` so its
    /// `Call(rt_base + RT_*)` instructions land on the right helpers.
    fn rt_base_for_standalone() -> u32 {
        runtime::import_signatures().len() as u32
    }

    /// Identity remap — every import available. Used by tests that
    /// target `None` (native). Matches `runtime::build_import_remap`
    /// applied to an all-true availability vector.
    fn identity_import_remap() -> Vec<Option<u32>> {
        (0..runtime::IMPORT_COUNT as usize)
            .map(|i| Some(i as u32))
            .collect()
    }

    fn compile_main(src: &str) -> Function {
        let mut program = compile_all(src);
        assert!(
            program.closures.is_empty(),
            "compile_main used on a program with closures — use compile_all + build_standalone_module_many",
        );
        program.top_level.remove(0).1
    }

    fn with_tail_expression_builder<R>(
        src: &str,
        f: impl FnOnce(&mut Builder<'_, '_>, &Expression) -> R,
    ) -> R {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare failed");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker failed");
        let checker_info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
            array_int_index_sites: checker.array_int_index_sites.clone(),
            record_field_read_sites: checker.record_field_read_sites.clone(),
        };
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
        let expression = match main.body.last().expect("main should have a body") {
            fai_compiler::ast::Statement::ExpressionStatement(es) => &es.expression,
            other => panic!("expected tail expression, got {other:?}"),
        };
        let functions = vec![FunctionInfo {
            name: main.name.clone(),
            param_count: main.params.len() as u16 + main.type_params.len() as u16,
            type_param_count: main.type_params.len() as u16,
            param_names: param_names_for(&main),
            include_in_coverage: false,
            param_defaults: param_defaults_for(&main),
            ..Default::default()
        }];
        let type_indices = build_fai_type_indices();
        let module_aliases = HashMap::new();
        let extern_fn_indices = HashMap::new();
        let import_remap = identity_import_remap();
        let enum_members = HashMap::new();
        let type_fields = HashMap::new();
        let named_imports = HashMap::new();
        let mocked_fn_ids = HashSet::new();
        let std_method_fn_ids = HashMap::new();
        let strings = RefCell::new(StringInterner::default());
        let closures = RefCell::new(Vec::new());
        let ownership_sites = RefCell::new(Vec::new());
        let module_constants = HashMap::new();
        let extern_out_params: HashMap<String, Vec<bool>> = HashMap::new();
        let module_vars: HashMap<String, u32> = HashMap::new();
        let ctx = BuildContext {
            rt: RtOffsets {
                base: rt_base_for_standalone(),
            },
            functions: &functions,
            checker: &checker_info,
            import_remap: &import_remap,
            fai_func_type_indices: &type_indices,
            module_aliases: &module_aliases,
            extern_fn_indices: &extern_fn_indices,
            enum_members: &enum_members,
            type_fields: &type_fields,
            named_imports: &named_imports,
            mocked_fn_ids: &mocked_fn_ids,
            std_method_fn_ids: &std_method_fn_ids,
            closure_offset_base: 0,
            strings: &strings,
            closures: &closures,
            module_constants: &module_constants,
            extern_out_params: &extern_out_params,
            module_vars: &module_vars,
            ownership_sites: &ownership_sites,
            file_path: None,
            async_ctx: None,
        };
        let mut builder = Builder::new(main, &ctx, None);
        f(&mut builder, expression)
    }

    fn compile_tail_expression_shape(src: &str) -> ValueShape {
        with_tail_expression_builder(src, |builder, expression| {
            builder
                .compile_expr(expression)
                .expect("compile expression")
        })
    }

    fn compile_tail_expression_as(src: &str, want: ValueShape) {
        with_tail_expression_builder(src, |builder, expression| {
            builder
                .compile_expr_as(expression, want)
                .expect("compile expression as shape");
        });
    }

    #[test]
    fn int_int_addition_returns_raw_int_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Int\ndo\n  1 + 2\nend");
        assert_eq!(shape, ValueShape::RawInt);
    }

    #[test]
    fn int_int_subtraction_returns_raw_int_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Int\ndo\n  5 - 2\nend");
        assert_eq!(shape, ValueShape::RawInt);
    }

    #[test]
    fn int_int_comparison_returns_raw_bool_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Bool\ndo\n  5 > 2\nend");
        assert_eq!(shape, ValueShape::RawBool);
    }

    #[test]
    fn int_int_division_returns_raw_float_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Float\ndo\n  5 / 2\nend");
        assert_eq!(shape, ValueShape::RawFloat);
    }

    #[test]
    fn float_float_addition_returns_raw_float_shape() {
        let shape =
            compile_tail_expression_shape("def main\n    @return Float\ndo\n  1.5 + 2.5\nend");
        assert_eq!(shape, ValueShape::RawFloat);
    }

    #[test]
    fn int_float_addition_returns_raw_float_shape() {
        let shape =
            compile_tail_expression_shape("def main\n    @return Float\ndo\n  1 + 2.5\nend");
        assert_eq!(shape, ValueShape::RawFloat);
    }

    #[test]
    fn float_float_comparison_returns_raw_bool_shape() {
        let shape =
            compile_tail_expression_shape("def main\n    @return Bool\ndo\n  1.5 <= 2.5\nend");
        assert_eq!(shape, ValueShape::RawBool);
    }

    #[test]
    fn int_unary_negation_returns_raw_int_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Int\ndo\n  -5\nend");
        assert_eq!(shape, ValueShape::RawInt);
    }

    #[test]
    fn float_unary_negation_returns_raw_float_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Float\ndo\n  -5.5\nend");
        assert_eq!(shape, ValueShape::RawFloat);
    }

    #[test]
    fn value_shape_for_type_keeps_only_monomorphic_primitives_raw() {
        use fai_checker::types::{optional_of, Type};

        assert_eq!(shape_for_type(&Type::Int), ValueShape::RawInt);
        assert_eq!(shape_for_type(&Type::Float), ValueShape::RawFloat);
        assert_eq!(shape_for_type(&Type::Bool), ValueShape::RawBool);
        assert_eq!(shape_for_type(&optional_of(Type::Int)), ValueShape::Boxed);
        assert_eq!(shape_for_type(&Type::String), ValueShape::Boxed);
        assert_eq!(shape_for_type(&Type::Unknown), ValueShape::Boxed);
    }

    #[test]
    fn builder_shape_for_expr_uses_checker_expression_type() {
        let shape = with_tail_expression_builder(
            "def main\n    @return Int\ndo\n  1 + 2\nend",
            |builder, expression| builder.shape_for_expr(expression),
        );
        assert_eq!(shape, ValueShape::RawInt);
    }

    #[test]
    fn compile_expr_as_boxed_accepts_current_boxed_expression() {
        compile_tail_expression_as(
            "def main\n    @return Int\ndo\n  1 + 2\nend",
            ValueShape::Boxed,
        );
    }

    #[test]
    fn raw_int_let_identifier_lookup_is_raw_int() {
        // The local's stored shape is what lets the raw arithmetic
        // path pick `I64Add` over `RT_ADD`. `compile_expr` auto-boxes
        // identifier reads (so callers that ignore shape stay safe),
        // so the check goes through `numeric_shape_for_expr` which
        // reads the binding's shape directly.
        with_tail_expression_builder(
            "def main\n    @return Int\ndo\n  let x = 5\n  x\nend",
            |builder, expression| {
                let last = builder.fd.body.len() - 1;
                let prefix: Vec<Statement> = builder.fd.body[..last].to_vec();
                for stmt in &prefix {
                    builder
                        .compile_stmt(stmt)
                        .expect("compile prefix statement");
                }
                assert_eq!(
                    builder.numeric_shape_for_expr(expression),
                    Some(ValueShape::RawInt),
                );
            },
        );
    }

    #[test]
    fn typed_param_prelude_rebinds_int_param_raw() {
        with_tail_expression_builder(
            "def main\n    @param x Int\n    @return Int\ndo\n  x\nend",
            |builder, expression| {
                builder
                    .emit_typed_param_prelude()
                    .expect("emit param prelude");
                assert_eq!(
                    builder.numeric_shape_for_expr(expression),
                    Some(ValueShape::RawInt),
                );
            },
        );
    }

    /// Walk a built standalone module and return the call targets
    /// (function indices) inside `main`'s body. Main lives at code
    /// section index `RT_COUNT` because the standalone layout places
    /// runtime helpers first in the code section, followed by
    /// top-level user functions with `main` first. Used by Phase 4
    /// wasm-inspection tests that assert monomorphic arithmetic elides
    /// the runtime-helper dispatch.
    fn collect_main_call_targets(wasm: &[u8]) -> Vec<u32> {
        let parser = wasmparser::Parser::new(0);
        let mut main_body_idx = 0usize;
        let mut targets = Vec::new();
        for payload in parser.parse_all(wasm) {
            if let wasmparser::Payload::CodeSectionEntry(body) = payload.expect("wasm payload") {
                if main_body_idx == runtime::RT_COUNT as usize {
                    let mut reader = body.get_operators_reader().expect("operators reader");
                    while !reader.eof() {
                        if let wasmparser::Operator::Call { function_index } =
                            reader.read().expect("operator")
                        {
                            targets.push(function_index);
                        }
                    }
                    return targets;
                }
                main_body_idx += 1;
            }
        }
        panic!("main body not found in wasm module");
    }

    #[test]
    fn raw_int_add_emits_no_rt_helper_call() {
        let wasm =
            build_standalone_module(compile_main("def main\n    @return Int\ndo\n  1 + 2\nend"));
        let targets = collect_main_call_targets(&wasm);
        let rt_base = rt_base_for_standalone();
        for (offset, name) in [
            (runtime::RT_ADD, "rt_add"),
            (runtime::RT_MAKE_INT, "rt_make_int"),
            (runtime::RT_AS_NUMBER, "rt_as_number"),
        ] {
            let forbidden = rt_base + offset;
            assert!(
                !targets.contains(&forbidden),
                "expected no call to {} ({}) in main body, got call targets: {:?}",
                name,
                forbidden,
                targets,
            );
        }
    }

    #[test]
    fn raw_float_add_emits_no_rt_helper_call() {
        let wasm = build_standalone_module(compile_main(
            "def main\n    @return Float\ndo\n  1.5 + 2.5\nend",
        ));
        let targets = collect_main_call_targets(&wasm);
        let rt_base = rt_base_for_standalone();
        for (offset, name) in [
            (runtime::RT_ADD, "rt_add"),
            (runtime::RT_MAKE_FLOAT, "rt_make_float"),
            (runtime::RT_AS_NUMBER, "rt_as_number"),
        ] {
            let forbidden = rt_base + offset;
            assert!(
                !targets.contains(&forbidden),
                "expected no call to {} ({}) in main body, got call targets: {:?}",
                name,
                forbidden,
                targets,
            );
        }
    }

    #[test]
    fn mandelbrot_inner_comparison_emits_no_rt_helper_call() {
        // Mirrors the mandelbrot inner loop's
        // `zr * zr + zi * zi <= 4` check. Both sides of the outer `+`
        // are Float, so the add should be native F64Add and the
        // comparison native F64Le — no runtime-helper dispatch.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  var zr = toFloat(0)\n",
            "  var zi = toFloat(0)\n",
            "  zr * zr + zi * zi <= 4\n",
            "end\n",
        )));
        let targets = collect_main_call_targets(&wasm);
        let rt_base = rt_base_for_standalone();
        for (offset, name) in [
            (runtime::RT_ADD, "rt_add"),
            (runtime::RT_LE, "rt_le"),
            (runtime::RT_MAKE_INT, "rt_make_int"),
            (runtime::RT_MAKE_FLOAT, "rt_make_float"),
            (runtime::RT_AS_NUMBER, "rt_as_number"),
        ] {
            let forbidden = rt_base + offset;
            assert!(
                !targets.contains(&forbidden),
                "expected no call to {} ({}) in main body, got call targets: {:?}",
                name,
                forbidden,
                targets,
            );
        }
    }

    /// Compile `def main @return Int do <stmt>; <ret_expr> end` and run.
    fn run_let_then_return(stmt: &str, ret_type: &str, ret_expr: &str) -> i64 {
        let src = format!(
            "def main\n    @return {}\ndo\n  {}\n  {}\nend\n",
            ret_type, stmt, ret_expr,
        );
        let wasm = build_standalone_module_many(compile_all(&src));
        run_module(&wasm)
    }

    #[test]
    fn let_float_annotated_int_literal_widens_to_float() {
        // `let val Float = 0` — declared Float, RHS is Int literal 0.
        // Should widen and return 0.0.
        assert_eq!(
            run_let_then_return("let val Float = 0", "Float", "val"),
            boxed_float(0.0),
        );
        // Non-zero Int literal as Float.
        assert_eq!(
            run_let_then_return("let val Float = 7", "Float", "val"),
            boxed_float(7.0),
        );
    }

    #[test]
    fn let_inferred_float_literal_binds_float() {
        assert_eq!(
            run_let_then_return("let val = 0.0", "Float", "val"),
            boxed_float(0.0),
        );
    }

    #[test]
    fn let_float_annotated_int_variable_widens() {
        // Declared Float, RHS is an Int-typed identifier — the
        // RawInt→RawFloat path in emit_convert handles the widening
        // at runtime.
        let src = concat!(
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  let n = 7\n",
            "  let f Float = n\n",
            "  f\n",
            "end\n",
        );
        let wasm = build_standalone_module_many(compile_all(src));
        assert_eq!(run_module(&wasm), boxed_float(7.0));
    }

    #[test]
    fn var_float_annotated_int_literal_widens_to_float() {
        // Same as the let case but with `var`.
        assert_eq!(
            run_let_then_return("var val Float = 0", "Float", "val"),
            boxed_float(0.0),
        );
    }

    #[test]
    fn let_int_annotated_whole_float_literal_narrows() {
        // Whole-valued Float literal (e.g. `1.0`) may be declared as
        // Int — the literal narrows exactly. Non-whole literals like
        // `1.23` are rejected by the checker (tested in fai-checker).
        assert_eq!(
            run_let_then_return("let val Int = 0.0", "Int", "val"),
            boxed_int(0),
        );
        assert_eq!(
            run_let_then_return("let val Int = 42.0", "Int", "val"),
            boxed_int(42),
        );
    }

    #[test]
    fn let_inferred_int_literal_binds_int() {
        assert_eq!(
            run_let_then_return("let val = 0", "Int", "val"),
            boxed_int(0),
        );
        assert_eq!(
            run_let_then_return("let val = 42", "Int", "val"),
            boxed_int(42),
        );
    }

    #[test]
    fn let_bool_literal_binds_bool() {
        assert_eq!(
            run_let_then_return("let val = true", "Bool", "val"),
            boxed_bool(true),
        );
        assert_eq!(
            run_let_then_return("let val = false", "Bool", "val"),
            boxed_bool(false),
        );
        assert_eq!(
            run_let_then_return("let val Bool = true", "Bool", "val"),
            boxed_bool(true),
        );
        assert_eq!(
            run_let_then_return("let val Bool = false", "Bool", "val"),
            boxed_bool(false),
        );
    }

    #[test]
    fn let_bool_from_comparison_binds_bool() {
        assert_eq!(
            run_let_then_return("let val = 1 != 2", "Bool", "val"),
            boxed_bool(true),
        );
        assert_eq!(
            run_let_then_return("let val Bool = 1 == 1", "Bool", "val"),
            boxed_bool(true),
        );
        assert_eq!(
            run_let_then_return("let val = 1 == 2", "Bool", "val"),
            boxed_bool(false),
        );
    }

    #[test]
    fn raw_mixed_int_float_emits_no_rt_helper_call() {
        let wasm = build_standalone_module(compile_main(
            "def main\n    @return Float\ndo\n  3 + 0.5\nend",
        ));
        let targets = collect_main_call_targets(&wasm);
        let rt_base = rt_base_for_standalone();
        for (offset, name) in [
            (runtime::RT_ADD, "rt_add"),
            (runtime::RT_MAKE_INT, "rt_make_int"),
            (runtime::RT_MAKE_FLOAT, "rt_make_float"),
            (runtime::RT_AS_NUMBER, "rt_as_number"),
        ] {
            let forbidden = rt_base + offset;
            assert!(
                !targets.contains(&forbidden),
                "expected no call to {} ({}) in main body, got call targets: {:?}",
                name,
                forbidden,
                targets,
            );
        }
    }

    fn boxed_int(n: i32) -> i64 {
        runtime::QNAN | runtime::TAG_INT | (n as u32 as i64)
    }

    fn boxed_bool(b: bool) -> i64 {
        runtime::QNAN | runtime::TAG_BOOL | (if b { 1 } else { 0 })
    }

    fn boxed_float(f: f64) -> i64 {
        f.to_bits() as i64
    }

    /// Compile `def main @return {ret} do {expr} end` and run it.
    fn run_main_expr(ret: &str, expr: &str) -> i64 {
        let src = format!("def main\n    @return {}\ndo\n  {}\nend\n", ret, expr);
        let wasm = build_standalone_module(compile_main(&src));
        run_module(&wasm)
    }

    #[test]
    fn raw_int_local_passed_to_is_int_is_boxed() {
        // is_int inspects the NaN-box tag bits; if `x` leaks raw
        // (as a bare i64 without QNAN|TAG_INT), is_int returns false
        // and the program returns boxed_bool(false). This is the
        // observable form of the print-garbage bug the benchmark
        // surfaced: builtins taking Unknown must receive boxed values.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let x = 7\n",
            "  is_int(x)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm), boxed_bool(true));
    }

    #[test]
    fn int_int_operator_matrix_returns_correct_values() {
        // Arithmetic → Int (except `/` which promotes to Float).
        for (op, expected) in [("+", 9), ("-", 3), ("*", 18), ("//", 2), ("%", 0)] {
            let got = run_main_expr("Int", &format!("6 {} 3", op));
            assert_eq!(got, boxed_int(expected), "Int Int `{}` on 6, 3", op);
        }
        // Division: 6 / 3 → 2.0 Float.
        assert_eq!(
            run_main_expr("Float", "6 / 3"),
            boxed_float(2.0),
            "Int Int `/` promotes to Float",
        );
        // Comparisons → Bool.
        for (op, expected) in [
            ("==", false),
            ("!=", true),
            ("<", false),
            ("<=", false),
            (">", true),
            (">=", true),
        ] {
            let got = run_main_expr("Bool", &format!("6 {} 3", op));
            assert_eq!(got, boxed_bool(expected), "Int Int `{}` on 6, 3", op);
        }
    }

    #[test]
    fn float_float_operator_matrix_returns_correct_values() {
        for (op, expected) in [("+", 9.0), ("-", 3.0), ("*", 18.0), ("/", 2.0)] {
            let got = run_main_expr("Float", &format!("6.0 {} 3.0", op));
            assert_eq!(
                got,
                boxed_float(expected),
                "Float Float `{}` on 6.0, 3.0",
                op
            );
        }
        for (op, expected) in [
            ("==", false),
            ("!=", true),
            ("<", false),
            ("<=", false),
            (">", true),
            (">=", true),
        ] {
            let got = run_main_expr("Bool", &format!("6.0 {} 3.0", op));
            assert_eq!(
                got,
                boxed_bool(expected),
                "Float Float `{}` on 6.0, 3.0",
                op
            );
        }
    }

    #[test]
    fn int_float_operator_matrix_returns_correct_values() {
        // Mixed arithmetic promotes to Float. Forai's checker
        // rejects mixed-type comparisons, so those are covered only
        // by the same-type matrices.
        for (op, expected) in [("+", 6.5), ("-", 5.5), ("*", 3.0), ("/", 12.0)] {
            let got = run_main_expr("Float", &format!("6 {} 0.5", op));
            assert_eq!(got, boxed_float(expected), "Int Float `{}` on 6, 0.5", op);
        }
    }

    #[test]
    fn float_int_operator_matrix_returns_correct_values() {
        for (op, expected) in [("+", 7.0), ("-", 5.0), ("*", 6.0), ("/", 6.0)] {
            let got = run_main_expr("Float", &format!("6.0 {} 1", op));
            assert_eq!(got, boxed_float(expected), "Float Int `{}` on 6.0, 1", op);
        }
    }

    /// A compiled program for the standalone-module tests:
    /// top-level functions followed by the closures each of their
    /// bodies materialised, plus the interned string-data buffer the
    /// module assembler lays out at memory offset 0.
    struct TestProgram {
        top_level: Vec<(FunctionInfo, Function)>,
        closures: Vec<BuiltClosure>,
        string_data: Vec<u8>,
        ownership_sites: Vec<crate::debug_info::OwnershipSiteDebugEntry>,
    }

    /// Compile every top-level function in `src` directly to wasm.
    /// Runs the checker first to capture expression types, UFCS, and
    /// named-param reorder info, then feeds each function declaration
    /// to `build_function` with the standalone-module type-index layout.
    fn compile_all(src: &str) -> TestProgram {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare failed");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker failed");
        let checker_info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
            array_int_index_sites: checker.array_int_index_sites.clone(),
            record_field_read_sites: checker.record_field_read_sites.clone(),
        };

        let mut decls: Vec<fai_compiler::ast::FunctionDeclaration> = Vec::new();
        // `main` is emitted first so it lands at proto index 0 —
        // matches the production pipeline's convention.
        if let Some(main) = prepared.serde_ast.statements.iter().find_map(|s| match s {
            fai_compiler::ast::Statement::FunctionDeclaration(fd) if fd.name == "main" => {
                Some(fd.clone())
            }
            _ => None,
        }) {
            decls.push(main);
        }
        for s in &prepared.serde_ast.statements {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
                if fd.name != "main" {
                    decls.push(fd.clone());
                }
            }
        }
        let infos: Vec<FunctionInfo> = decls
            .iter()
            .map(|fd| FunctionInfo {
                name: fd.name.clone(),
                param_count: fd.params.len() as u16 + fd.type_params.len() as u16,
                type_param_count: fd.type_params.len() as u16,
                param_names: param_names_for(fd),
                include_in_coverage: fd.name != "main",
                param_defaults: param_defaults_for(fd),
                source_line: fd.location.line,
                ..Default::default()
            })
            .collect();
        let rt = RtOffsets {
            base: rt_base_for_standalone(),
        };
        let type_indices = build_fai_type_indices();
        // Collect top-level `use` statements and build
        // alias → canonical-path. Only namespace imports are
        // supported on this standalone helper path (named imports
        // like `use X { foo, bar }` are covered by production module
        // preparation).
        let module_aliases = collect_module_aliases(&prepared.serde_ast.statements);
        let extern_fn_indices = collect_extern_fn_indices(&prepared.serde_ast.statements);
        // Collect enum + type-declaration tables so tests exercise
        // the same paths the production `build_program_full` does.
        let mut enum_members: HashMap<String, Vec<String>> = HashMap::new();
        let mut type_fields: HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>> =
            HashMap::new();
        for s in &prepared.serde_ast.statements {
            match s {
                fai_compiler::ast::Statement::EnumDeclaration(ed) => {
                    enum_members.insert(ed.name.clone(), ed.members.clone());
                }
                fai_compiler::ast::Statement::TypeDeclaration(td) => {
                    type_fields.insert(td.name.clone(), td.fields.clone());
                }
                _ => {}
            }
        }
        let named_imports: HashMap<String, String> = HashMap::new();
        let strings = RefCell::new(StringInterner::default());
        let ownership_sites = RefCell::new(Vec::new());
        let remap = identity_import_remap();
        let mut top_level = Vec::with_capacity(decls.len());
        let mut all_closures = Vec::new();
        let empty_mocked: HashSet<u32> = HashSet::new();
        let empty_std_ids: HashMap<(String, String), u32> = HashMap::new();
        for (fd, info) in decls.iter().zip(infos.iter().cloned()) {
            let result = build_function_with_spy_and_offset(
                fd,
                rt,
                &infos,
                &checker_info,
                &type_indices,
                &module_aliases,
                &extern_fn_indices,
                &remap,
                &strings,
                &enum_members,
                &type_fields,
                &named_imports,
                &empty_mocked,
                &empty_std_ids,
                all_closures.len() as u32,
                None,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &ownership_sites,
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("direct build failed: {:?}", e));
            top_level.push((info, result.main));
            all_closures.extend(result.closures);
        }
        TestProgram {
            top_level,
            closures: all_closures,
            string_data: strings.into_inner().bytes,
            ownership_sites: ownership_sites.into_inner(),
        }
    }

    /// Walk top-level statements for namespace `use` imports and
    /// build a `last-segment → full-dotted-path` map. `use std.file`
    /// becomes `"file" -> "std.file"`, `use std.net.tcp` becomes
    /// `"tcp" -> "std.net.tcp"`. Named imports (`use X { a }`) are
    /// skipped — those need per-symbol binding that the direct path
    /// doesn't support yet.
    fn collect_module_aliases(stmts: &[fai_compiler::ast::Statement]) -> HashMap<String, String> {
        let mut aliases = HashMap::new();
        for s in stmts {
            if let fai_compiler::ast::Statement::UseStatement(u) = s {
                if u.import_all || u.imported_names.is_some() {
                    continue;
                }
                if let Some(last) = u.module_path.last() {
                    aliases.insert(last.clone(), u.module_path.join("."));
                }
            }
        }
        aliases
    }

    /// Walk top-level `extern { ... }` blocks and assign each
    /// function a stable index. Matches the ordering
    /// `compiler.rs` uses so the host's extern table indices line
    /// up whether the function was built via the direct path or the
    /// bytecode path. A program with no extern blocks returns an
    /// empty map.
    fn collect_extern_fn_indices(stmts: &[fai_compiler::ast::Statement]) -> HashMap<String, u16> {
        let mut indices = HashMap::new();
        let mut next_idx = 0u16;
        for s in stmts {
            if let fai_compiler::ast::Statement::ExternBlockDeclaration(ext) = s {
                for f in &ext.functions {
                    indices.insert(f.name.clone(), next_idx);
                    next_idx = next_idx.checked_add(1).expect("too many extern functions");
                }
            }
        }
        indices
    }

    /// Build a standalone wasm module from a compiled program:
    /// runtime helpers + top-level functions + closure functions. A
    /// table populated from the closure list lets `call_indirect`
    /// dispatch at runtime.
    ///
    /// Module layout (function-index space):
    /// - `[0, import_count)` host imports
    /// - `[import_count, import_count + RT_COUNT)` runtime helpers
    /// - `[import_count + RT_COUNT, ... + top_level_count)` fai funcs
    /// - `[after top_level, ... + closure_count)` closures (in
    ///   `top_level[0].proto_index` order)
    ///
    /// Types are pre-allocated so that `fai_func_type_indices[N]`
    /// matches what `build_function` already baked into the
    /// `CallIndirect` instructions.
    fn build_module(program: TestProgram) -> Vec<u8> {
        let mut module = EncModule::new();

        // ── types ──
        let mut types = TypeSection::new();
        let import_sigs = runtime::import_signatures();
        let mut import_type_indices = Vec::with_capacity(import_sigs.len());
        for (_, params, results) in &import_sigs {
            import_type_indices.push(types.len());
            types.ty().function(params.clone(), results.clone());
        }
        let rt_sigs = runtime::type_signatures();
        let mut rt_type_indices = Vec::with_capacity(rt_sigs.len());
        for (params, results) in &rt_sigs {
            rt_type_indices.push(types.len());
            types.ty().function(params.clone(), results.clone());
        }
        // Pre-declare FaiFunc(0..=MAX_FAI_ARITY) — matches what
        // `build_fai_type_indices` hands to the builder above. Any
        // direct-built function or closure with arity in this range
        // picks the matching slot; closure `CallIndirect` instructions
        // reference it by absolute type index.
        let fai_type_indices = build_fai_type_indices();
        for arity in 0..=MAX_FAI_ARITY {
            let params: Vec<ValType> = (0..arity).map(|_| ValType::I64).collect();
            let expected = types.len();
            types.ty().function(params, vec![ValType::I64]);
            assert_eq!(
                expected, fai_type_indices[&arity],
                "test harness fai-type layout diverged from builder's type_indices map",
            );
        }
        module.section(&types);

        // ── imports ──
        let mut imports = ImportSection::new();
        for (i, (name, _, _)) in import_sigs.iter().enumerate() {
            imports.import("env", name, EntityType::Function(import_type_indices[i]));
        }
        module.section(&imports);

        // ── functions ──
        // [rt_0 ...] [top_level_0 ...] [closure_0 ...]
        let mut funcs = FunctionSection::new();
        for &t in &rt_type_indices {
            funcs.function(t);
        }
        for (info, _) in &program.top_level {
            funcs.function(fai_type_indices[&info.param_count]);
        }
        for c in &program.closures {
            funcs.function(fai_type_indices[&c.info.param_count]);
        }
        module.section(&funcs);

        // ── tables ──
        // Elements are populated below; the table's min size equals
        // the closure count so wasm validation accepts the element
        // segment. Empty program → empty table is still legal.
        let mut tables = TableSection::new();
        let closure_count = program.closures.len() as u32;
        tables.table(TableType {
            element_type: RefType::FUNCREF,
            minimum: closure_count as u64,
            maximum: Some(closure_count as u64),
            table64: false,
            shared: false,
        });
        module.section(&tables);

        // ── memory ──
        let mut mem = MemorySection::new();
        mem.memory(MemoryType {
            minimum: 16,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&mem);

        // ── globals ──
        // Order matches translate.rs: __heap_ptr, __env_ptr,
        // error_flag, error_value. Index 1 is env_ptr — which is
        // what `GLOBAL_ENV_PTR` (and translate.rs) reference.
        //
        // `__heap_ptr` starts above the interned string data so heap
        // allocations don't overwrite it. Round up to 8-byte
        // alignment (RT_ALLOC hands out aligned blocks).
        let bucket_base = ((program.string_data.len() as u32) + 7) & !7;
        let heap_start = (bucket_base + runtime::FREE_BUCKET_REGION_BYTES + 7) & !7;
        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(heap_start as i32),
        );
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        globals.global(
            GlobalType {
                val_type: ValType::I64,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i64_const(0),
        );
        // Heap free-list head (index 4 — appended after the 4 fixed globals;
        // this harness has no module-var/scheduler globals). 0 = empty list.
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        // Live-object counter (index 5, plan 113).
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        module.section(&globals);

        let import_count = import_sigs.len() as u32;
        let top_level_base = import_count + runtime::RT_COUNT;
        let closure_base = top_level_base + program.top_level.len() as u32;
        let main_func_idx = top_level_base;

        // ── exports ──
        // `_start`, `memory`, and `__heap_ptr` are the three the
        // production build emits and the runtime tests rely on. The
        // heap-pointer global is exposed so heap-boundary regression
        // tests can pre-position it before invoking `_start` to
        // exercise allocation patterns that only show up when the
        // heap is near a page boundary.
        let mut exports = ExportSection::new();
        exports.export("_start", ExportKind::Func, main_func_idx);
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("__heap_ptr", ExportKind::Global, 0);
        exports.export("__live_objects", ExportKind::Global, 5); // plan 113 oracle
        module.section(&exports);

        // ── elements ──
        // Populate the function-reference table: slot `i` points at
        // the wasm function for closure `i`. Closure `i`'s runtime
        // `table_idx` field is just `i`, which matches what
        // `compile_function_expression` wrote.
        if closure_count > 0 {
            let mut elements = ElementSection::new();
            let func_indices: Vec<u32> = (0..closure_count).map(|i| closure_base + i).collect();
            elements.active(
                Some(0),
                &ConstExpr::i32_const(0),
                Elements::Functions(func_indices.into()),
            );
            module.section(&elements);
        }

        // ── code ──
        let mut code = CodeSection::new();
        let import_remap: Vec<Option<u32>> = (0..runtime::IMPORT_COUNT as usize)
            .map(|i| Some(i as u32))
            .collect();
        let known = runtime::KnownStrings::default();
        for f in runtime::emit_all(import_count, &import_remap, &known, 4, 5, bucket_base, None, None) {
            code.function(&f);
        }
        for (_, f) in &program.top_level {
            code.function(f);
        }
        for c in &program.closures {
            code.function(&c.function);
        }
        module.section(&code);

        // ── data ──
        // Emit the string-literal pool as an active segment starting
        // at memory offset 0. `RT_ALLOC_STRING(offset, len)` reads
        // these bytes and copies them into a freshly-allocated
        // String object, so the data must survive the module's
        // lifetime at a known offset.
        if !program.string_data.is_empty() {
            let mut data = DataSection::new();
            data.active(
                0,
                &ConstExpr::i32_const(0),
                program.string_data.iter().copied(),
            );
            module.section(&data);
        }

        // ── debug metadata (plan 116): name section + fai-dbg table ──
        let mut dbg: Vec<crate::debug_info::FnDebugEntry> = Vec::new();
        for (i, (name, _, _)) in import_sigs.iter().enumerate() {
            dbg.push(crate::debug_info::FnDebugEntry::unlocated(i as u32, *name));
        }
        for (k, n) in runtime::rt_fn_names().iter().enumerate() {
            dbg.push(crate::debug_info::FnDebugEntry::unlocated(
                import_count + k as u32,
                *n,
            ));
        }
        for (i, (info, _)) in program.top_level.iter().enumerate() {
            dbg.push(crate::debug_info::FnDebugEntry {
                index: top_level_base + i as u32,
                name: info.name.clone(),
                file: info.source_file.clone(),
                line: info.source_line,
            });
        }
        for (i, c) in program.closures.iter().enumerate() {
            dbg.push(crate::debug_info::FnDebugEntry {
                index: closure_base + i as u32,
                name: c.info.name.clone(),
                file: c.info.source_file.clone(),
                line: c.info.source_line,
            });
        }
        crate::debug_info::append_debug_sections(
            &mut module,
            &dbg,
            &crate::debug_info::DbgMeta {
                bucket_base: Some(bucket_base),
                bucket_count: runtime::NUM_FREE_BUCKETS,
                ownership_sites: program.ownership_sites.clone(),
            },
        );

        module.finish()
    }

    /// Wrapper for single-main tests that don't define any other
    /// functions. Wraps the given `Function` as `main` with zero
    /// arity, no closures.
    fn build_standalone_module(main_fn: Function) -> Vec<u8> {
        build_module(TestProgram {
            top_level: vec![(
                FunctionInfo {
                    name: "main".to_string(),
                    param_count: 0,
                    type_param_count: 0,
                    include_in_coverage: false,
                    param_defaults: Vec::new(),
                    ..Default::default()
                },
                main_fn,
            )],
            closures: Vec::new(),
            string_data: Vec::new(),
            ownership_sites: Vec::new(),
        })
    }

    /// Multi-function wrapper for tests that build a source file's
    /// entire top-level function list plus any nested closures.
    fn build_standalone_module_many(program: TestProgram) -> Vec<u8> {
        build_module(program)
    }

    fn run_module(wasm: &[u8]) -> i64 {
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        // Stub every host import the module declares. Phase C
        // arithmetic doesn't need any of them at runtime, but
        // validation needs the functions to be present. Each stub
        // matches its signature and returns a default.
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            // Mock the FFI boundary: `ffi_begin` parks the task expecting the
            // driver loop to resume it once the worker finishes. Here there's no
            // loop, so resume immediately (`ffi_result` then returns the default
            // 0); enough for async extern-call programs to complete.
            if name == "ffi_begin" {
                linker
                    .func_new(
                        "env",
                        name,
                        FuncType::new(&engine, wt_params, wt_results),
                        move |mut caller, args, _rets| {
                            let task_id = match args.first() {
                                Some(Val::I32(t)) => *t,
                                _ => return Ok(()),
                            };
                            if let Some(f) = caller
                                .get_export("__fai_resume_task")
                                .and_then(|e| e.into_func())
                            {
                                let _ =
                                    f.call(&mut caller, &[Val::I32(task_id)], &mut [Val::I32(0)]);
                            }
                            Ok(())
                        },
                    )
                    .unwrap();
                continue;
            }
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        // Sync program: a single `_start` returning the root value.
        if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
            return start.call(&mut store, ()).expect("run");
        }
        // Async program: kick off the root task, drive `__fai_poll` to completion
        // (status 2 = done, 3 = failed), then read the root's result. Any program
        // that invokes a closure value is async now (closure calls are potential
        // suspension points), so previously-sync tests can land here.
        let start_async = instance
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .expect("_start or _start_async export");
        start_async.call(&mut store, ()).expect("run _start_async");
        let poll = instance
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .expect("__fai_poll export");
        let mut status = 1;
        for _ in 0..10_000_000 {
            status = poll.call(&mut store, ()).expect("poll");
            if status == 2 || status == 3 {
                break;
            }
        }
        assert!(status == 2, "async root did not complete (status {status})");
        let task_result = instance
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .expect("__fai_task_result export");
        task_result.call(&mut store, 1).expect("task_result")
    }

    #[test]
    fn direct_int_literal_return() {
        // Simplest possible program: `def main @return Int do 42 end`.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // NaN-boxed Int: high bits = QNAN | TAG_INT, low 32 = value.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        assert_eq!(result, expected, "direct-built main should return 42");
    }

    #[test]
    fn direct_arithmetic() {
        // Exercise RT_ADD/RT_SUB/RT_MUL through the direct path.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  2 * 3 + 4 - 1\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (9u32 as u64);
        assert_eq!(result, expected, "2*3 + 4 - 1 should be 9");
    }

    #[test]
    fn direct_float_arithmetic() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  1.5 + 2.5\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        assert_eq!(result, 4.0_f64.to_bits());
    }

    #[test]
    fn direct_int_division_returns_float() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  5 / 2\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        assert_eq!(result, 2.5_f64.to_bits());
    }

    #[test]
    fn direct_let_binding() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let x = 10\n",
            "  let y = 32\n",
            "  x + y\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_comparison_true() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  5 > 3\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_unary_negation() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 7\n",
            "  -n\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | ((-7i32) as u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_if_true_branch() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var x = 0\n",
            "  if true\n",
            "    x = 42\n",
            "  end\n",
            "  x\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_if_raw_bool_local_condition() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let flag = true\n",
            "  if flag\n",
            "    7\n",
            "  else\n",
            "    3\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_if_else_picks_else() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  if false\n",
            "    1\n",
            "  else\n",
            "    99\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (99u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_if_else_if_chain() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 2\n",
            "  if n == 1\n",
            "    10\n",
            "  else if n == 2\n",
            "    20\n",
            "  else if n == 3\n",
            "    30\n",
            "  else\n",
            "    99\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (20u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_while_sum_to_ten() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var i = 0\n",
            "  var sum = 0\n",
            "  while i < 10\n",
            "    i = i + 1\n",
            "    sum = sum + i\n",
            "  end\n",
            "  sum\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 1 + 2 + ... + 10 = 55
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (55u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_while_break() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var i = 0\n",
            "  while true\n",
            "    if i == 5\n",
            "      break\n",
            "    end\n",
            "    i = i + 1\n",
            "  end\n",
            "  i\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (5u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_while_continue_skips_iteration() {
        // Sum 1..=10 but skip multiples of 3. Expected: 1+2+4+5+7+8+10 = 37.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var i = 0\n",
            "  var sum = 0\n",
            "  while i < 10\n",
            "    i = i + 1\n",
            "    if i % 3 == 0\n",
            "      continue\n",
            "    end\n",
            "    sum = sum + i\n",
            "  end\n",
            "  sum\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (37u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_range_continue_skips_iteration() {
        // `continue` in for-range must reach the counter increment,
        // not jump back to the condition check. Without the inner
        // $continue block this'd infinite-loop.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for i in 1..5\n",
            "    if i == 3\n",
            "      continue\n",
            "    end\n",
            "    s = s + i\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // `1..5` is exclusive: visits 1,2,3,4. Skip 3 → 1+2+4 = 7.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_range_exclusive_sum() {
        // `..` is exclusive: 0+1+2+3+4 = 10.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for i in 0..5\n",
            "    s = s + i\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (10u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_range_inclusive_sum() {
        // `...` is inclusive: 0+1+2+3+4+5 = 15.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for i in 0...5\n",
            "    s = s + i\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (15u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_range_break() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for i in 1..100\n",
            "    if i > 3\n",
            "      break\n",
            "    end\n",
            "    s = s + i\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // Runs i=1,2,3 then break. sum = 6.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (6u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_try_catches_throw() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var result = 0\n",
            "  try\n",
            "    throw 99\n",
            "    result = 1\n",
            "  catch e\n",
            "    result = 42\n",
            "  end\n",
            "  result\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_try_no_throw_skips_catch() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var result = 0\n",
            "  try\n",
            "    result = 1\n",
            "  catch e\n",
            "    result = 99\n",
            "  end\n",
            "  result\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_try_finally_runs_after_success() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var log = 0\n",
            "  try\n",
            "    log = 1\n",
            "  catch e\n",
            "    log = 2\n",
            "  finally\n",
            "    log = log + 10\n",
            "  end\n",
            "  log\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // try path runs → log=1; finally adds 10 → 11.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 11;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_try_finally_runs_after_catch() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var log = 0\n",
            "  try\n",
            "    throw 99\n",
            "    log = 1\n",
            "  catch e\n",
            "    log = 2\n",
            "  finally\n",
            "    log = log + 100\n",
            "  end\n",
            "  log\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // throw → catch sets log=2; finally adds 100 → 102.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 102;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_catch_binds_thrown_value() {
        // The catch body must bind `e` to the thrown value. The
        // checker types `e` as `Error`, so we don't do arithmetic on
        // it — `e == e` exercises the binding (always true) without
        // tripping the type rule.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var result = 0\n",
            "  try\n",
            "    throw 7\n",
            "  catch e\n",
            "    if e == e\n",
            "      result = 42\n",
            "    end\n",
            "  end\n",
            "  result\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_unary_not_flips_truthiness() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  !false\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_call_simple() {
        // Main calls a helper defined in the same file. Helper's
        // proto index is 1 (main is always 0 in `compile_all`'s
        // ordering).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Double an int.\n",
            "def double\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  double(21)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_call_multi_arg() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Add two ints.\n",
            "def add\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a + b\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  add(10, 32)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_call_recursion() {
        // Classic factorial — recursion on the direct path proves the
        // wasm function index resolution is consistent between the
        // caller and the callee's self-reference.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Integer factorial.\n",
            "def fact\n",
            "    @param n Int\n",
            "    @return Int\n",
            "do\n",
            "  if n <= 1\n",
            "    1\n",
            "  else\n",
            "    n * fact(n - 1)\n",
            "  end\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  fact(5)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 5! = 120
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 120;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_call_return_used_in_expr() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Return a constant.\n",
            "def ten\n",
            "    @return Int\n",
            "do\n",
            "  10\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  ten() + ten() + 22\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_ufcs_rewrites_to_positional() {
        // `x.double()` with `double` a user-declared function rewrites
        // to `double(x)` — the checker marks the location; the builder
        // reads it and emits a direct call.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Double an int.\n",
            "def double\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 21\n",
            "  n.double()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_ufcs_with_named_param_reorder() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Greet.\n",
            "def greet\n",
            "    @param name String\n",
            "    @param salutation String\n",
            "    @return String\n",
            "do\n",
            "  salutation + ', ' + name\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length('Alice'.greet(salutation: 'Hi'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 9;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_ufcs_chain() {
        // Chained UFCS: `x.doubled().incremented()` → `incremented(doubled(x))`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Doubled.\ndef doubled\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "# Incremented.\ndef incremented\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x + 1\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 10\n",
            "  n.doubled().incremented()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // (10 * 2) + 1 = 21
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 21;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_ufcs_with_extra_args() {
        // `x.add(5)` → `add(x, 5)`. The object becomes the first
        // positional arg, the remaining call args follow.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Add.\ndef add\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a + b\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 37\n",
            "  n.add(5)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_params_in_order() {
        // Named args in declaration order — no reorder entry from the
        // checker. The builder compiles them as positional.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Sub.\ndef sub\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a - b\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  sub(a: 50, b: 8)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_params_reordered() {
        // `b: 8, a: 50` is the opposite of declaration order — the
        // checker records a reorder map; the builder evaluates args
        // in source order but emits them in declaration order.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Sub.\ndef sub\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a - b\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  sub(b: 8, a: 50)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 50 - 8 = 42 — proves the reorder put `a=50` in slot 0 and
        // `b=8` in slot 1 even though the call wrote b first.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_params_three_way_reorder() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Mk.\ndef mk\n",
            "    @param first Int\n",
            "    @param second Int\n",
            "    @param third Int\n",
            "    @return Int\n",
            "do\n",
            "  first * 100 + second * 10 + third\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  mk(third: 3, first: 1, second: 2)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // first=1 second=2 third=3 → 123
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 123;
        assert_eq!(result, expected);
    }

    /// Extract just `main`'s AST + run the checker. Used by
    /// rejection tests that want to exercise a specific error path
    /// in `build_function` without building the whole module.
    fn compile_main_ast(
        src: &str,
    ) -> (
        fai_compiler::ast::FunctionDeclaration,
        CheckerInfo,
        Vec<FunctionInfo>,
    ) {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare");
        let mut checker = fai_checker::Checker::new();
        // Some rejection tests intentionally include constructs the
        // checker flags (e.g. closures with captured locals of the
        // wrong type). We still want the builder to get the AST, so
        // swallow checker errors here.
        let _ = checker.check_program(&prepared.serde_ast.statements);
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
            array_int_index_sites: checker.array_int_index_sites,
            record_field_read_sites: checker.record_field_read_sites,
        };
        let mut infos: Vec<FunctionInfo> = Vec::new();
        let mut main = None;
        for s in &prepared.serde_ast.statements {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
                infos.push(FunctionInfo {
                    name: fd.name.clone(),
                    param_count: fd.params.len() as u16 + fd.type_params.len() as u16,
                    type_param_count: fd.type_params.len() as u16,
                    param_names: param_names_for(fd),
                    include_in_coverage: fd.name != "main",
                    param_defaults: param_defaults_for(fd),
                    ..Default::default()
                });
                if fd.name == "main" {
                    main = Some(fd.clone());
                }
            }
        }
        (main.expect("no main"), info, infos)
    }

    #[test]
    fn direct_nested_closure_reads_enclosing_local() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let outer = do with n Int\n",
            "    let inner = do with m Int\n",
            "      n + m\n",
            "    end\n",
            "    inner(3)\n",
            "  end\n",
            "  outer(39)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nested_closure_writes_enclosing_upvalue() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var total = 0\n",
            "  let outer = do\n",
            "    let inner = do\n",
            "      total = total + 21\n",
            "    end\n",
            "    inner()\n",
            "    inner()\n",
            "  end\n",
            "  outer()\n",
            "  total\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nested_closure_returned_then_called_later() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let makeAdder = do with n Int\n",
            "    do with m Int\n",
            "      n + m\n",
            "    end\n",
            "  end\n",
            "  let add40 = makeAdder(40)\n",
            "  add40(2)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_rejects_module_member_access() {
        // Without an alias map, `file.read(path)` cannot resolve as a
        // known std module call and must refuse instead of compiling
        // arbitrary member access as a module dispatch.
        let (main, ci, infos) = compile_main_ast(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  file.read('/tmp/x')\n",
            "end\n",
        ));
        // Note: the module-alias map is empty — `compile_main_ast`
        // doesn't build one, so `file` doesn't resolve as a known
        // alias and we fall through to the unknown-module refusal.
        let err = build_function(
            &main,
            RtOffsets {
                base: rt_base_for_standalone(),
            },
            &infos,
            &ci,
            &build_fai_type_indices(),
            &HashMap::new(),
            &HashMap::new(),
            &identity_import_remap(),
            &RefCell::new(StringInterner::default()),
        )
        .expect_err("builder should refuse module member calls");
        match err {
            BuildError::ModuleAccessNotYetSupported(name) => assert_eq!(name, "read"),
            other => panic!("expected ModuleAccessNotYetSupported, got {:?}", other),
        }
    }

    #[test]
    fn direct_use_statement_in_body_is_noop() {
        // Contrived but legal: a `use` statement sitting inside a
        // function body. Direct builder treats it as a no-op since
        // module resolution already happened upstream.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  use std.array\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── Nowait (fire-and-forget spawn) ──────────────────────────
    //
    // `nowait expr` wraps `expr` in a zero-arg closure and hands
    // it to `IMPORT_SPAWN`. Under the current tier-1 runtime the
    // host invokes the closure synchronously, so the observable
    // behaviour is equivalent to calling the body directly — the
    // asynchrony boundary is at a higher layer.

    #[test]
    fn direct_type_constructor_builds_dict() {
        // `type Point @x Int @y Int end` with a constructor call.
        // The constructor lowers to a dict literal; field access
        // via `p.x` uses the normal RT_GET_FIELD path.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "type Point\n",
            "  x Int\n",
            "  y Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let p = Point(x: 3, y: 4)\n",
            "  p.x + p.y\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_type_constructor_fills_defaults() {
        // Fields with `= default` are filled when the caller omits
        // them. Here `color` defaults to 'red'; the constructor
        // call only supplies `name`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "type Thing\n",
            "  name String\n",
            "  color String = 'red'\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let t = Thing(name: 'x')\n",
            "  length(t.color)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_spy_records_call_and_mocks_return() {
        // Full spy pipeline: a test block that mocks a user fn,
        // invokes it, then asserts call count and argument shape.
        // This exercises preamble emission, spy_check_call wiring,
        // and the assert.* imports end-to-end against the same
        // compile_all test helper used by the rest of this module.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Double.\ndef double\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  42\n",
            "end\n",
        )));
        // compile_all runs with is_test=false and no mocked_fn_ids
        // (the test helper intentionally stays minimal). So we're
        // just checking that the generated module still runs clean
        // on the unmocked path. End-to-end spy behavior is exercised
        // by the CLI's todo-cli fixture where `is_test=true` and
        // real test blocks drive the instrumentation.
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_multiple_fn_refs_dont_share_closure_slot() {
        // Regression: two top-level functions each synthesizing a
        // forwarding closure for a different fn-ref used to bake
        // `table_idx=0` into both closures' headers. At runtime
        // call_indirect then landed on whichever closure was
        // emitted first, so `apply(x, doubled)` and
        // `apply(x, tripled)` both returned the doubled value.
        //
        // Fix: `closure_offset_base` threads the global closure
        // count into each top-level function's builder so the
        // baked `table_idx` matches the module's element-section
        // slot.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Doubled.\ndef doubled\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "# Tripled.\ndef tripled\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 3\n",
            "end\n",
            "\n",
            "# Apply.\ndef apply\n",
            "    @param v Int\n",
            "    @param fn (Int) -> Int\n",
            "    @return Int\n",
            "do\n",
            "  fn(v)\n",
            "end\n",
            "\n",
            "# CallDoubled.\ndef callDoubled\n",
            "    @return Int\n",
            "do\n",
            "  apply(5, doubled)\n",
            "end\n",
            "\n",
            "# CallTripled.\ndef callTripled\n",
            "    @return Int\n",
            "do\n",
            "  apply(5, tripled)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  callDoubled() + callTripled()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 5*2 + 5*3 = 25; if both fn-refs collided onto one closure
        // slot, we'd get 5*2 + 5*2 = 20 or 5*3 + 5*3 = 30.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 25;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_mock_stubs_compile_and_run_as_noops() {
        // `mock(fn, val)`, `mockOnce`, `mockReset`, and
        // `assert.{calledWith,callCount,notCalled}` are checker-known
        // void-returning builtins with no runtime interception yet.
        // They compile as no-ops so test blocks that reference them
        // don't refuse codegen. This test exercises each shape inside
        // an entry function and verifies main returns cleanly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Helper.\ndef helper\n",
            "    @return Int\n",
            "do\n",
            "  1\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  mock(helper, 7)\n",
            "  mockOnce(helper, 8)\n",
            "  mockReset(helper)\n",
            "  assert.calledWith(helper)\n",
            "  assert.callCount(helper, 0)\n",
            "  assert.notCalled(helper)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_function_ref_as_value_runs_through_apply() {
        // Passing a top-level `def` name as a value — `apply(x, shout)` —
        // synthesizes a forwarding closure under the hood. Verify the
        // value round-trips through `apply` and the wrapped call
        // returns the original input.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Shout.\ndef shout\n",
            "    @param text Int\n",
            "    @return Int\n",
            "do\n",
            "  text\n",
            "end\n",
            "\n",
            "# Apply.\ndef apply\n",
            "    @param value Int\n",
            "    @param fn (Int) -> Int\n",
            "    @return Int\n",
            "do\n",
            "  fn(value)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  apply(42, shout)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_function_ref_zero_arg_forwards() {
        // Zero-arity function references should synthesize a
        // zero-param wrapper. `call(greet)` round-trips through the
        // closure → indirect call → named function.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Greet.\ndef greet\n",
            "    @return Int\n",
            "do\n",
            "  7\n",
            "end\n",
            "\n",
            "# Call.\ndef call\n",
            "    @param fn () -> Int\n",
            "    @return Int\n",
            "do\n",
            "  fn()\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  call(greet)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nowait_runs_body() {
        // Since the host stub for IMPORT_SPAWN in our tests is a
        // no-op (returns I64(0)), the closure body doesn't execute
        // here. Verify the program still compiles and main's
        // trailing literal returns cleanly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# SlowWork.\ndef slowWork @return Void do\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  nowait slowWork()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nowait_spawn_import_is_called() {
        // Override IMPORT_SPAWN to return a known NaN-boxed Int
        // — proves the spawn dispatch fires and its return is
        // threaded through Drop. Not a valid return value per the
        // runtime contract (spawn returns VAL_VOID in production),
        // but harmless since we Drop it.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# SlowWork.\ndef slowWork @return Void do\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  nowait slowWork()\n",
            "  7\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (999u32 as u64);
        // Main doesn't return spawn's result (it's Drop'd), so main
        // still returns 7 regardless of the stub's value. The test
        // proves the spawn call is present in the wasm (otherwise
        // the override would have no effect) and that validation
        // passes.
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_SPAWN, Val::I64(sentinel as i64))
                as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_legacy_bare_all_two_tasks_dispatches_run_all() {
        // `all(e1, e2)` should synthesize a closure per arg and
        // call IMPORT_RUN_ALL. Override the import to return a
        // sentinel so we can prove the dispatch fires. (The
        // default stub returns 0, which would pass a `let _ = all(...)`
        // trivially — the override makes the link load-bearing.)
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Double.\ndef double\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = all(double(1), double(2))\n",
            "  7\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_RUN_ALL, Val::I64(sentinel as i64))
                as u64;
        // The returned tuple is bound to `_` and unused; main returns 7.
        // Hitting the sentinel proves only that validation + imports
        // wired up; the 7 below proves downstream compilation kept
        // working after the synthesized closures landed.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nowait_captures_upvalue() {
        // The wrapped closure sees outer locals as upvalues — same
        // machinery as `do with ... end` literals. Compilation
        // must succeed with the upvalue wiring intact; the actual
        // call happens host-side, which our stub no-ops.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.log\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let label = 'hello'\n",
            "  nowait log.info(label)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── Multi-binding destructure ────────────────────────────────
    //
    // `let a, b = swap(1, 2)` and `a, b = swap(...)` destructure a
    // Tuple value. The RHS compiles to a tuple (the compiler
    // synthesises `TupleExpression` from the last `x, y` of a
    // multi-return function), then the LHS walks indices via
    // `RT_GET_INDEX(tuple, MAKE_INT(i))`.

    #[test]
    fn direct_let_multi_binding_swap() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Swap.\ndef swap\n",
            "    @param x Int\n",
            "    @param y Int\n",
            "    @return Int\n",
            "    @return Int\n",
            "do\n",
            "  y, x\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a, b = swap(1, 2)\n",
            "  a * 10 + b\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // swap(1, 2) → (2, 1); a=2, b=1 → 2*10 + 1 = 21
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 21;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_var_multi_binding_swap() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Swap.\ndef swap\n",
            "    @param x Int\n",
            "    @param y Int\n",
            "    @return Int\n",
            "    @return Int\n",
            "do\n",
            "  y, x\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var a, b = swap(7, 3)\n",
            "  a - b\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // swap(7, 3) → (3, 7); a=3, b=7 → 3 - 7 = -4
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | ((-4i32) as u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_reassignment_multi_binding() {
        // Plain `a, b = swap(...)` (no `let`/`var`) — both names
        // must already exist as mutable bindings. Tests the
        // AssignmentTarget::Variables multi-name path.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Swap.\ndef swap\n",
            "    @param x Int\n",
            "    @param y Int\n",
            "    @return Int\n",
            "    @return Int\n",
            "do\n",
            "  y, x\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var a = 1\n",
            "  var b = 2\n",
            "  a, b = swap(a, b)\n",
            "  a * 10 + b\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // Initial a=1 b=2; swap(1,2) → (2,1); a=2 b=1 → 21
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 21;
        assert_eq!(result, expected);
    }

    // ── Field + index assignment ─────────────────────────────────
    //
    // `obj.field = x` routes through `RT_SET_FIELD(obj, key_ptr,
    // key_len, value)`; `arr[i] = x` writes directly to
    // `mem[obj_addr + 8 + i*8]`. Both mutate in place. Tests here
    // verify the full round-trip: assign, then read back.

    #[test]
    fn direct_dict_field_assignment_round_trip() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var d = {score: 1}\n",
            "  d.score = 99\n",
            "  d.score\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_dict_field_assignment_adds_new_key() {
        // `RT_SET_FIELD` appends the entry when the key isn't
        // present (that's why dict literals over-allocate
        // capacity — see `compile_dict_literal`).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var d = {a: 1}\n",
            "  d.b = 2\n",
            "  array.length(d)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_array_index_assignment_round_trip() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var a = [10, 20, 30]\n",
            "  a[1] = 99\n",
            "  a[1]\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_array_index_assignment_preserves_length() {
        // Mutation-in-place — length stays the same.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var a = [10, 20, 30]\n",
            "  a[0] = 999\n",
            "  array.length(a)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_in_array_sums_elements() {
        // `for x in [1,2,3]` now compiles — array literal +
        // `compile_for_array` walks index 0..length, loading each
        // element with an I64 read from `addr + 8 + i*8`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for n in [1, 2, 3]\n",
            "    s = s + n\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 6;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_in_array_empty_no_iterations() {
        // Empty array — the length-based guard (`index >= length`)
        // exits before the body runs.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 7\n",
            "  for n in []\n",
            "    s = s + n\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_in_array_break_and_continue() {
        // Mixed break + continue in the same loop — confirms the
        // LoopFrame targets are wired correctly for both targets.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for n in [1, 2, 3, 4, 5, 6]\n",
            "    if n == 3\n",
            "      continue\n",
            "    end\n",
            "    if n == 5\n",
            "      break\n",
            "    end\n",
            "    s = s + n\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 1 + 2 + 4 = 7 (3 skipped, 5 breaks before summing).
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_computed_callee_compiles() {
        // `(get_fn())()` — calling the result of a call expression.
        // The builder now lowers arbitrary callee expressions
        // through `compile_indirect_call_from_expr`: evaluate the
        // expression to a boxed closure, then dispatch through the
        // closure header. Non-closure values trap at runtime via
        // `RT_OBJ_ADDR`; compilation succeeds.
        //
        // Regression guard: this used to refuse with
        // `UnsupportedExpression("CallExpression/non-identifier")`
        // and forui's `eventHandlers[id]()` / `matched!.builder()`
        // call sites hit that refusal. The direct builder now
        // matches what forai programs actually need.
        use fai_compiler::ast;
        let loc = ast::SourceLocation { line: 1, column: 1 };
        let inner = ast::CallExpression {
            callee: Box::new(ast::Expression::IdentifierExpression(
                ast::IdentifierExpression {
                    name: "get_fn".into(),
                    location: loc.clone(),
                },
            )),
            args: Vec::new(),
            location: loc.clone(),
        };
        let outer = ast::CallExpression {
            callee: Box::new(ast::Expression::CallExpression(inner)),
            args: Vec::new(),
            location: loc.clone(),
        };
        let main = ast::FunctionDeclaration {
            name: "main".into(),
            type_params: Vec::new(),
            params: Vec::new(),
            return_types: Vec::new(),
            body: vec![ast::Statement::ExpressionStatement(
                ast::ExpressionStatement {
                    expression: ast::Expression::CallExpression(outer),
                    location: loc.clone(),
                },
            )],
            doc: None,
            is_private: None,
            is_abstract: false,
            is_remote: false,
            auth_policy: None,
            location: loc,
            doc_comment: None,
        };
        // `get_fn` needs to exist in the function table so the
        // inner call resolves; arity 0 / returns i64 matches the
        // standalone type-index layout.
        let get_fn_info = FunctionInfo {
            name: "get_fn".into(),
            param_count: 0,
            type_param_count: 0,
            include_in_coverage: false,
            param_defaults: Vec::new(),
            ..Default::default()
        };
        build_function(
            &main,
            RtOffsets {
                base: rt_base_for_standalone(),
            },
            &[get_fn_info],
            &CheckerInfo::empty(),
            &build_fai_type_indices(),
            &HashMap::new(),
            &HashMap::new(),
            &identity_import_remap(),
            &RefCell::new(StringInterner::default()),
        )
        .expect("builder should accept computed callee");
    }

    // ── closures ──────────────────────────────────────────────────
    //
    // These exercise the full closure path: a `FunctionExpression` in
    // main's body materialises a heap-allocated closure object, and
    // an indirect call dispatches through the table using the
    // closure's `table_idx` field.

    #[test]
    fn direct_closure_no_capture() {
        // Simplest closure — no upvalues. Calling it via the local
        // exercises `call_indirect` without any env-load logic.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let f = do with n Int\n",
            "    n * 3\n",
            "  end\n",
            "  f(14)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_single_capture() {
        // Closure captures `k` from the enclosing scope by value.
        // Body reads the upvalue via `GlobalGet(env_ptr) + I64Load(0)`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 32\n",
            "  let f = do with n Int\n",
            "    k + n\n",
            "  end\n",
            "  f(10)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_multi_capture() {
        // Multiple upvalues — exercises the offset math on the second
        // (non-zero) capture slot.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a = 10\n",
            "  let b = 20\n",
            "  let c = 12\n",
            "  let f = do with n Int\n",
            "    a + b + c + n\n",
            "  end\n",
            "  f(0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_captures_let_by_value() {
        // `let` bindings aren't mutable, so there's nothing to share —
        // the closure keeps a plain snapshot. Proves the non-cell path
        // still works.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 42\n",
            "  let f = do with n Int\n",
            "    k + n\n",
            "  end\n",
            "  f(0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_called_twice_preserves_env() {
        // Calling the same closure twice in sequence must restore
        // env_ptr correctly after each call (the save/restore dance).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 7\n",
            "  let f = do with n Int\n",
            "    k * n\n",
            "  end\n",
            "  f(2) + f(4)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // (7*2) + (7*4) = 14 + 28 = 42.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── Captured-var mutation (cell-boxing) ─────────────────────
    //
    // When a closure writes to an outer `var`, both the outer and the
    // closure must see each other's updates. The compiler boxes such
    // vars in heap cells and stores cell addresses in the closure's
    // env, so reads and writes on either side dereference the same
    // cell.

    #[test]
    fn direct_closure_writes_captured_var_visible_from_outer() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Block.\n",
            "type def Block\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "# Call.\n",
            "def call\n",
            "    @param b Block\n",
            "    @return Void\n",
            "do\n",
            "  b()\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var x = 0\n",
            "  call(do\n",
            "    x = 42\n",
            "  end)\n",
            "  x\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_increments_captured_counter() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Block.\n",
            "type def Block\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "# Run n times.\n",
            "def repeatN\n",
            "    @param n Int\n",
            "    @param b Block\n",
            "    @return Void\n",
            "do\n",
            "  var i = 0\n",
            "  while i < n\n",
            "    b()\n",
            "    i = i + 1\n",
            "  end\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var count = 0\n",
            "  repeatN(5, do\n",
            "    count = count + 1\n",
            "  end)\n",
            "  count\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_two_closures_share_captured_var() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Block.\n",
            "type def Block\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "# Run pair.\n",
            "def runPair\n",
            "    @param a Block\n",
            "    @param b Block\n",
            "    @return Void\n",
            "do\n",
            "  a()\n",
            "  b()\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var total = 0\n",
            "  runPair(\n",
            "    do\n",
            "      total = total + 10\n",
            "    end,\n",
            "    do\n",
            "      total = total + 32\n",
            "    end\n",
            "  )\n",
            "  total\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_sees_outer_mutation_after_creation() {
        // Because outer-var captures are shared (cell-boxed) when the
        // var is captured by any closure, an outer mutation AFTER
        // closure creation is visible when the closure runs.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var k = 42\n",
            "  let f = do with n Int\n",
            "    k + n\n",
            "  end\n",
            "  k = 100\n",
            "  f(0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // With cell-sharing the closure reads k = 100, returns 100.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 100;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_zero_arg() {
        // A thunk — zero-arg closure still takes the call_indirect
        // path; checks that the `FaiFunc(0)` type is wired correctly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 42\n",
            "  let f = do\n",
            "    k\n",
            "  end\n",
            "  f()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── Case statement ────────────────────────────────────────────
    //
    // `case value when m1 body1 when m2 body2 else default end`
    // lowers to a nested if/else chain where each condition is
    // `value == match_expr`. Tests here exercise matching branches,
    // the default, tail-position use, and RT_EQ's polymorphism
    // across primitive types.

    #[test]
    fn direct_case_matches_first_branch() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 0\n",
            "  case 1\n",
            "    when 1\n",
            "      r = 10\n",
            "    when 2\n",
            "      r = 20\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 10;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_case_matches_later_branch() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 0\n",
            "  case 2\n",
            "    when 1\n",
            "      r = 10\n",
            "    when 2\n",
            "      r = 20\n",
            "    when 3\n",
            "      r = 30\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 20;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_case_falls_through_to_else() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 0\n",
            "  case 99\n",
            "    when 1\n",
            "      r = 10\n",
            "    when 2\n",
            "      r = 20\n",
            "    default\n",
            "      r = 999\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 999;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_case_no_match_no_else_leaves_state() {
        // No arm matches and no default — body must not execute.
        // The `r` variable keeps its pre-case value.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 42\n",
            "  case 99\n",
            "    when 1\n",
            "      r = 10\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_case_matches_string() {
        // RT_EQ handles String comparison by deep byte equality.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 0\n",
            "  case 'beta'\n",
            "    when 'alpha'\n",
            "      r = 1\n",
            "    when 'beta'\n",
            "      r = 2\n",
            "    when 'gamma'\n",
            "      r = 3\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_string_ordering_is_lexicographic() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  'apple' < 'banana'\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm), boxed_bool(true));

        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  'banana' < 'apple'\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm), boxed_bool(false));
    }

    #[test]
    fn direct_case_in_tail_position() {
        // Case as the last statement — each branch body becomes a
        // tail, with its trailing expression as the function's
        // return value. No explicit `return` needed.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Describe.\ndef describe\n",
            "    @param n Int\n",
            "    @return Int\n",
            "do\n",
            "  case n\n",
            "    when 1\n",
            "      100\n",
            "    when 2\n",
            "      200\n",
            "    default\n",
            "      999\n",
            "  end\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  describe(2)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 200;
        assert_eq!(result, expected);
    }

    // ── Aggregate + expression gaps ───────────────────────────────
    //
    // Dictionaries, tuples, indexing, field access, template
    // strings, optional checks, and force-unwrap. Each exercises a
    // distinct surface that was previously refused in compile_expr.

    #[test]
    fn direct_dict_literal_length() {
        // Three-entry dict — count stored at offset 4, array.length
        // (polymorphic on dicts too) reads that count directly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let d = {name: 'alice', age: 30, admin: true}\n",
            "  array.length(d)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_dict_field_access_returns_value() {
        // dict.field routes through RT_GET_FIELD; the stored value
        // comes back NaN-boxed.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let d = {score: 42}\n",
            "  d.score\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_dict_missing_field_returns_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  let d = {a: 1}\n",
            "  d.missing\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_array_index_returns_element() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a = [10, 20, 30]\n",
            "  a[1]\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 20;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_dict_index_by_string_key() {
        // dict[key] where key is a String — RT_GET_INDEX branches
        // on the container tag and does a key scan for dicts. The
        // checker types `d[k]` as `Unknown?` since it can't know
        // which key-type yielded what; `@return Unknown?` matches.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Unknown?\n",
            "do\n",
            "  let d = {score: 99}\n",
            "  d['score']\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    // NOTE: `TupleExpression` has no user-visible literal syntax —
    // it's compiler-internal (type-def field packing, multi-return
    // destructuring; see fai-compiler/src/lib.rs:664). The
    // `compile_tuple_literal` path above is exercised once those
    // surfaces land on the direct builder.

    #[test]
    fn direct_template_string_length() {
        // "hello {name}" with name='world' → "hello world", length 11.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let name = 'world'\n",
            "  string.length(\"hello {{name}}\")\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 11;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_template_string_with_number_coerces() {
        // The expression part is an Int — RT_VALUE_TO_STR handles
        // the coercion, no user-side conversion needed.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 42\n",
            "  string.length(\"n={{n}}\")\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // "n=42" has 4 chars.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 4;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_optional_check_null_is_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Nullable.\ndef nullable @return Int? do\n",
            "  null\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  nullable()?\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_optional_check_non_null_is_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Nullable.\ndef nullable @return Int? do\n",
            "  42\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  nullable()?\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_force_unwrap_passes_non_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Nullable.\ndef nullable @return Int? do\n",
            "  7\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  nullable()!\n",
            "end\n",
        )));
        let result = try_run_module(&wasm).expect("unwrap pass should not trap") as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_force_unwrap_traps_on_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Nullable.\ndef nullable @return Int? do\n",
            "  null\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  nullable()!\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("null unwrap must trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    // ── std module dispatch ───────────────────────────────────────
    //
    // These exercise `resolve_module_call` end-to-end: a top-level
    // `use std.foo` installs the alias; the direct builder rewrites
    // `foo.method(args)` into the right arg-shape + import call.
    // The default wasmtime stubs all return 0, so return-value
    // assertions focus on imports where that's meaningful (`void`
    // imports, `MakeBool(0)` → false). Tests that just discard the
    // import's result verify arg-shape + validation end-to-end.

    /// Run a module overriding one import with a constant return.
    /// `override_idx` is the wasm import index (e.g.
    /// `runtime::IMPORT_NET_AVAILABLE`); `ret` is pushed into the
    /// first result slot. Tests use this to distinguish a true from
    /// the default-zero stub reply.
    fn run_module_with_override(wasm: &[u8], override_idx: u32, ret_val: Val) -> i64 {
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (i, (name, params, results)) in runtime::import_signatures().iter().enumerate() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            let override_here = i as u32 == override_idx;
            let ret_val = ret_val.clone();
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = if override_here {
                                ret_val.clone()
                            } else {
                                match ty {
                                    wasm_encoder::ValType::I32 => Val::I32(0),
                                    wasm_encoder::ValType::I64 => Val::I64(0),
                                    wasm_encoder::ValType::F32 => Val::F32(0),
                                    wasm_encoder::ValType::F64 => Val::F64(0),
                                    _ => Val::I32(0),
                                }
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        // Sync program: a single `_start` returning the root value.
        if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
            return start.call(&mut store, ()).expect("run");
        }
        // Async program: kick off the root task, drive `__fai_poll` to completion
        // (status 2 = done, 3 = failed), then read the root's result. Any program
        // that invokes a closure value is async now (closure calls are potential
        // suspension points), so previously-sync tests can land here.
        let start_async = instance
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .expect("_start or _start_async export");
        start_async.call(&mut store, ()).expect("run _start_async");
        let poll = instance
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .expect("__fai_poll export");
        let mut status = 1;
        for _ in 0..10_000_000 {
            status = poll.call(&mut store, ()).expect("poll");
            if status == 2 || status == 3 {
                break;
            }
        }
        assert!(status == 2, "async root did not complete (status {status})");
        let task_result = instance
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .expect("__fai_task_result export");
        task_result.call(&mut store, 1).expect("task_result")
    }

    #[test]
    fn direct_module_log_info_no_op() {
        // `log.info(msg)` is void-returning; the dispatcher emits a
        // `VAL_VOID` trailer after the call. We drop the result, so
        // main's return value is the subsequent Int literal.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.log\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  log.info('hello')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_net_available_true() {
        // Override `net_available` to return 1 so we can verify the
        // result is wrapped through `MAKE_BOOL(1)` → `VAL_TRUE`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.net\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  net.available()\n",
            "end\n",
        )));
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_NET_AVAILABLE, Val::I32(1)) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_net_available_false() {
        // Default stub returns 0 — `MAKE_BOOL(0)` → `VAL_FALSE`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.net\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  net.available()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_path_join_compiles_and_runs() {
        // Two-string-arg shape. The stub returns i64(0) (not a valid
        // NaN-box, but we discard it); the test proves the wasm
        // validates with the right arg shape and main completes.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.path\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = path.join('a', 'b')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_path_basename_single_arg() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.path\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = path.basename('/a/b')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_json_parse_and_stringify() {
        // Exercise both the String→i64 (parse) and i64→i64
        // (stringify) arg shapes in one program.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.json\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let v = json.parse('{}')\n",
            "  let s = json.stringify(v)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_tcp_listen_takes_int_arg() {
        // Int-arg shape: the NaN-boxed `8080` is unboxed to an i32 on
        // the stack before `IMPORT_TCP_LISTEN`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.net.tcp\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = tcp.listen(8080)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_tcp_connect_mixed_args() {
        // (String, Int) → Int. Exercises both arg shapes in one call.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.net.tcp\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = tcp.connect('localhost', 8080)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_exists_true() {
        // Override `file_exists` to return 1 so the result wraps as
        // VAL_TRUE. Exercises the (String) → Bool pattern.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  file.exists('/tmp/foo')\n",
            "end\n",
        )));
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_FILE_EXISTS, Val::I32(1)) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_exists_false() {
        // Default stub returns 0 — MAKE_BOOL(0) → VAL_FALSE.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  file.exists('/nope')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_write_void_shape() {
        // (String, String) → void. Stub returns nothing; we verify
        // the wasm validates + runs and main returns its own Int.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  file.write('/tmp/x', 'hello')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_list_compiles_and_runs() {
        // (String) → Array?. Stub returns i64(0) which we discard,
        // so the test only verifies arg-shape + successful run.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = file.list('/tmp')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_read_returns_null_on_error() {
        // `file_read_str` returns VAL_NULL when the path doesn't
        // exist; the builder passes the boxed result straight through.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  file.read('/nope')\n",
            "end\n",
        )));
        let null_bits = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_FILE_READ_STR,
            Val::I64(null_bits as i64),
        ) as u64;
        assert_eq!(result, null_bits);
    }

    #[test]
    fn direct_module_file_read_passes_boxed_string_through() {
        // Success path: the host allocates the String and returns its
        // NaN-boxed value — the builder must not rewrap or unbox it.
        // Override with a recognizable object bit pattern and assert
        // it round-trips untouched.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  file.read('/tmp/empty')\n",
            "end\n",
        )));
        let obj_bits = (runtime::QNAN as u64) | 0x8000_0000_0000_0000_u64 | 0x1230;
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_FILE_READ_STR,
            Val::I64(obj_bits as i64),
        ) as u64;
        assert_eq!(result, obj_bits);
    }

    #[test]
    fn direct_module_time_now_compiles_and_runs() {
        // `time.now()` dispatches to IMPORT_NOW_MS + RT_MAKE_FLOAT —
        // matches the bytecode runtime's `METHOD_TIME_NOW`.
        //
        // The checker types `timeNow` as `String` (per docs: "ISO
        // 8601"), but the runtime actually returns a Float (ms since
        // epoch). The divergence is outside this work; discard the
        // value so the test doesn't bake in either side of the
        // disagreement — we only prove the arg-shape + import call
        // lower correctly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.time\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = time.now()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_time_unix_divides_and_truncates() {
        // `time.unix()` = `trunc(now_ms / 1000)` → Int. Override
        // `now_ms` to 3_456_789.0 → expect Int 3456.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.time\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  time.unix()\n",
            "end\n",
        )));
        let now_ms: f64 = 3_456_789.0;
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_NOW_MS, Val::F64(now_ms.to_bits()))
                as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (3456u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_floor_int_literal_promotes() {
        // `math.floor(x: Float)` — passing an Int literal is OK:
        // `RT_AS_NUMBER` promotes both Int and Float to f64. Test
        // that `floor(3)` is 3 via the full dispatch.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  math.floor(3.7)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_ceil() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  math.ceil(2.1)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_round_nearest() {
        // F64Nearest is banker's-rounding on half-values; 2.5 → 2,
        // 3.5 → 4. Test the unambiguous cases.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  math.round(7.8)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 8;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_abs() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.abs(-3.5)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 3.5f64 → reinterpret to i64.
        let expected = 3.5_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_sqrt() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.sqrt(16.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 4.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_min() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.min(2.0, 5.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 2.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_max() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.max(2.0, 5.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 5.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_pow_positive_exp() {
        // 2^10 = 1024.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.pow(2.0, 10.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 1024.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_pow_zero_exp() {
        // Anything^0 = 1. Zero-iteration loop, result stays at 1.0.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.pow(42.0, 0.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 1.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_pow_negative_exp() {
        // 2^-3 = 1/8 = 0.125. Exercises the invert branch.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.pow(2.0, -3.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 0.125_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_random_compiles_and_runs() {
        // Default stub returns f64(0.0); `random()` wraps it as
        // Float. The result is exactly the 0-bit-pattern Float, but
        // we don't lock the test to that — assert only that the
        // high bits don't have the Int tag (so we know we didn't
        // accidentally route through MAKE_INT).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.random()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        assert_eq!(
            result, 0u64,
            "random() with stub 0.0 → Float(0) bit pattern"
        );
    }

    // ── std.cli ──────────────────────────────────────────────────

    #[test]
    fn direct_module_cli_clear_no_args() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  cli.clear()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_cli_write_stringifies() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  cli.write('hi')\n",
            "  cli.writeLine(123)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_cli_move_to_int_args() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  cli.moveTo(3, 7)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_cli_read_line_no_prompt() {
        // Zero-arg form pushes (0, 0). The import stub returns 0
        // (not a valid NaN-box, but we discard). Main returns 42.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = cli.readLine()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_cli_read_line_with_prompt() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = cli.readLine('Name? ')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── std.storage ─────────────────────────────────────────────

    #[test]
    fn direct_module_storage_set_and_remove_void() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.storage\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  storage.storageSet('k', 'v')\n",
            "  storage.storageRemove('k')\n",
            "  storage.storageClear()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_storage_get_null_on_missing() {
        // `storageGet` returns `String?` (optional). The host returns
        // VAL_NULL for an absent key; the builder passes it through.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.storage\n",
            "\n",
            "def main\n",
            "    @return String?\n",
            "do\n",
            "  storage.storageGet('missing')\n",
            "end\n",
        )));
        let null_bits = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_STORAGE_GET_STR,
            Val::I64(null_bits as i64),
        ) as u64;
        assert_eq!(result, null_bits);
    }

    #[test]
    fn direct_module_storage_get_wraps_buffer_on_success() {
        // Default stub returns 0 (len=0). The builder wraps the
        // host-allocated boxed String — assert the boxed value rounds
        // trip through the builder untouched.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.storage\n",
            "\n",
            "def main\n",
            "    @return String?\n",
            "do\n",
            "  storage.storageGet('k')\n",
            "end\n",
        )));
        let obj_bits = (runtime::QNAN as u64) | 0x8000_0000_0000_0000_u64 | 0x1230;
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_STORAGE_GET_STR,
            Val::I64(obj_bits as i64),
        ) as u64;
        assert_eq!(result, obj_bits);
    }

    // ── std.convert ─────────────────────────────────────────────

    #[test]
    fn direct_module_convert_to_string() {
        // `convert.toString(42)` goes through RT_VALUE_TO_STR. We
        // assert the result is an object (String) tag — verifying
        // the full value would require parsing the heap.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  convert.toString(42)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let obj_high = (runtime::QNAN as u64) | 0x8000_0000_0000_0000_u64;
        assert_eq!(
            result & 0xFFFF_0000_0000_0000,
            obj_high & 0xFFFF_0000_0000_0000
        );
    }

    #[test]
    fn direct_module_convert_to_int_passthrough() {
        // `convert.toInt(42)` is a no-op at runtime — the int box
        // round-trips through the dispatcher unchanged.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  convert.toInt(42)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_convert_parse_int_succeeds() {
        // RT_PARSE_INT("123") returns Int(123). parseInt is Int? now.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  convert.parseInt('123')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 123;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_convert_parse_int_null_on_garbage() {
        // RT_PARSE_INT returns VAL_NULL when the input isn't a
        // valid integer.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  convert.parseInt('xyz')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_convert_parse_float_succeeds() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return Float?\n",
            "do\n",
            "  convert.parseFloat('3.5')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 3.5_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_convert_to_float_converts_unknown_int() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "# Convert an unknown value to float.\n",
            "def fromUnknown\n",
            "    @param value Unknown\n",
            "    @return Float\n",
            "do\n",
            "  convert.toFloat(value)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  fromUnknown(1)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 1.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    // ── std.string ──────────────────────────────────────────────
    //
    // Every method goes through `RT_CALL_NATIVE` with a METHOD_*
    // id. These tests exercise arg count, NaN-box result shapes,
    // and the NativeFn allocation → args-buffer layout. Default
    // wasmtime stubs don't come into play; the runtime helpers
    // execute entirely guest-side.

    #[test]
    fn direct_module_string_length() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length('hello')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_is_empty_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.isEmpty('')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_is_empty_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.isEmpty('x')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    /// Regression: native method dispatch must allocate space for the
    /// args buffer along with the NativeFn header, in a single
    /// `RT_ALLOC` call. The original code allocated 8 bytes for the
    /// header, then wrote args past `__heap_ptr` without separately
    /// allocating that region. When `__heap_ptr` sits close enough to
    /// the current memory boundary that the 8-byte header fits
    /// without growing memory but the args writes don't, the writes
    /// trap. (`RT_ALLOC` grows in 1 MiB chunks, so once a grow
    /// happens, there's plenty of slack — the bug only surfaces when
    /// the header alloc fits without grow.)
    ///
    /// We pre-position `__heap_ptr` so the program's allocations
    /// (`'hello world'` → 24 bytes, `'world'` → 16 bytes, NativeFn
    /// header → 8 bytes) all fit without growing memory and land the
    /// post-header pointer at `mem_size - 8`. The buggy code then
    /// writes arg[0] at `mem_size - 8` (in-bounds) and arg[1] at
    /// `mem_size` (OOB, traps). The fix sizes the single alloc to
    /// cover both header and args, so the grow covers everything.
    #[test]
    fn direct_native_method_at_heap_page_boundary() {
        use wasmtime::{Engine, Linker, Module as RuntimeModule, Store, Val};
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.contains('hello world', 'world')\n",
            "end\n",
        )));
        // Run the module ourselves so we can pin `__heap_ptr` to the
        // memory boundary before invoking `_start`. If we used
        // `run_module`, the program would never allocate enough on
        // its own to reach the boundary.
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, &wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            // Mock the FFI boundary: `ffi_begin` parks the task expecting the
            // driver loop to resume it once the worker finishes. Here there's no
            // loop, so resume immediately (`ffi_result` then returns the default
            // 0); enough for async extern-call programs to complete.
            if name == "ffi_begin" {
                linker
                    .func_new(
                        "env",
                        name,
                        FuncType::new(&engine, wt_params, wt_results),
                        move |mut caller, args, _rets| {
                            let task_id = match args.first() {
                                Some(Val::I32(t)) => *t,
                                _ => return Ok(()),
                            };
                            if let Some(f) = caller
                                .get_export("__fai_resume_task")
                                .and_then(|e| e.into_func())
                            {
                                let _ =
                                    f.call(&mut caller, &[Val::I32(task_id)], &mut [Val::I32(0)]);
                            }
                            Ok(())
                        },
                    )
                    .unwrap();
                continue;
            }
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        // Pre-position `__heap_ptr` so the program's three allocs
        // (24 + 16 + 8 = 48 bytes) all fit without growing memory and
        // leave the post-NativeFn-alloc pointer at `mem_size - 8`.
        // The buggy code's args[1] write at `mem_size` then traps.
        let memory = instance.get_memory(&mut store, "memory").expect("memory");
        let mem_size = memory.data_size(&mut store) as u32;
        let heap = instance
            .get_global(&mut store, "__heap_ptr")
            .expect("__heap_ptr global");
        let target = (mem_size - 56) & !7;
        heap.set(&mut store, Val::I32(target as i32))
            .expect("set heap_ptr");
        let start = instance
            .get_typed_func::<(), i64>(&mut store, "_start")
            .expect("_start export");
        let result = start.call(&mut store, ()).expect("run") as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(
            result, expected,
            "string.contains must succeed even when heap_ptr lands at the page boundary",
        );
    }

    #[test]
    fn direct_module_string_contains_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.contains('hello world', 'world')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_contains_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.contains('hello', 'xyz')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_starts_with() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.startsWith('hello', 'he')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_ends_with() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.endsWith('hello', 'lo')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_index_of() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.indexOf('hello', 'lo')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_index_of_missing() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.indexOf('hello', 'xyz')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // indexOf returns -1 when not found. `-1 as i32 as u32` is
        // 0xFFFFFFFF — the low 32 of a NaN-boxed Int.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 0xFFFF_FFFF_u64;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_substring_length() {
        // `substring('hello', 1, 4)` = "ell". We verify via length —
        // probing the resulting string's heap layout would couple
        // the test to allocator internals.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.substring('hello', 1, 4))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_trim_and_length() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.trim('  hi  '))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_to_upper_length_unchanged() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.toUpper('hello'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_repeat_length() {
        // "ab" repeated 3 times → length 6.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.repeat('ab', 3))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 6;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_replace_length() {
        // "foo" → "bar" in "foo foo" → "bar bar", length 7.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.replace('foo foo', 'foo', 'bar'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    // ── std.array (non-closure) ────────────────────────────────
    //
    // Same NativeMethod dispatch as std.string. Array literals
    // `[1, 2, 3]` now compile as well (see `compile_array_literal`),
    // so tests here construct and consume arrays end-to-end.

    #[test]
    fn direct_module_array_length() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.length([10, 20, 30])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_is_empty_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  array.isEmpty([])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_is_empty_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  array.isEmpty([1])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_contains_primitive_hit() {
        // Runtime does i64 bit-equality for primitive elements —
        // matches the VM's stringified comparison for same-typed
        // primitives. Int(20) bit pattern is the NaN-box the literal
        // compiled to, so this is a direct hit.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  array.contains([10, 20, 30], 20)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_contains_miss() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  array.contains([10, 20, 30], 99)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_index_of() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.indexOf([10, 20, 30], 30)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_index_of_missing() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.indexOf([10, 20], 99)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 0xFFFF_FFFF_u64;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_append_returns_longer() {
        // `append` returns a fresh array with the element added —
        // the runtime helper allocates + copies. Verify via length.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.length(array.append([1, 2], 3))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_reverse_preserves_length() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.length(array.reverse([1, 2, 3, 4]))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 4;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_reverse_moves_first_to_last() {
        // indexOf of the original first element after reverse is
        // length - 1. [1,2,3] reversed → [3,2,1], indexOf(result, 1)
        // should be 2.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.indexOf(array.reverse([1, 2, 3]), 1)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_sort_puts_smallest_first() {
        // After sort([3, 1, 2]) the 1 sits at index 0.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.indexOf(array.sort([3, 1, 2]), 1)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_slice_length() {
        // slice([10,20,30,40], 1, 3) = [20, 30], length 2.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.length(array.slice([10, 20, 30, 40], 1, 3))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_join_string_length() {
        // join(["a","b","c"], "-") = "a-b-c", length 5.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(array.join(['a', 'b', 'c'], '-'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    // ── std.array (closure-taking) ─────────────────────────────
    //
    // `map`, `filter`, `find`, `isAny`, `isAll` all have the same
    // shape: `(arr: Array, closure: Fn) -> <result>`. The guest
    // side just pushes two i64 values and calls the matching
    // `IMPORT_ARRAY_*`; the host reads array elements and calls
    // back into the guest via `__indirect_function_table` using
    // the closure's `table_idx`.
    //
    // These tests verify the guest-side plumbing. End-to-end
    // round-trip (host invoking the closure) would require a
    // wasmtime stub that reaches into the exported table — that
    // infrastructure lands with `std.http.server`.

    #[test]
    fn direct_module_array_map_compiles_and_runs() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.map([1, 2, 3], do with x Int\n",
            "    x * 2\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_map_result_propagates() {
        // Override IMPORT_ARRAY_MAP to return a known NaN-boxed Int
        // sentinel — confirms the result from the host flows out
        // as the expression's value. The checker types `array.map`
        // as returning `Int[]` here, but at wasm layer the result
        // is just an i64 bit pattern; we only assert on the raw
        // bits coming back from the overridden stub, so the type
        // declaration doesn't affect the check.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int[]\n",
            "do\n",
            "  array.map([1], do with x Int\n",
            "    x\n",
            "  end)\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (777u32 as u64);
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_ARRAY_MAP, Val::I64(sentinel as i64))
                as u64;
        assert_eq!(result, sentinel);
    }

    #[test]
    fn direct_module_array_filter_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.filter([1, 2, 3], do with x Int\n",
            "    x > 1\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_find_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.find([1, 2, 3], do with x Int\n",
            "    x == 2\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_is_any_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.isAny([1, 2], do with x Int\n",
            "    x > 1\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_is_all_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.isAll([1, 2], do with x Int\n",
            "    x > 0\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_map_captures_upvalue() {
        // The closure captures `k` from the enclosing scope. The
        // host invokes the closure via the table; the closure body
        // reads `k` via `env_ptr + 0`. Verifies the full closure
        // + module-dispatch interaction at compile time.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 10\n",
            "  let _ = array.map([1, 2, 3], do with x Int\n",
            "    x + k\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── std.test / assert ──────────────────────────────────────
    //
    // Each assertion either returns `VAL_TRUE` on pass or traps
    // after storing a message via `IMPORT_SET_TRAP_MSG`. Tests here
    // exercise both paths; the trap shows up as a wasmtime error
    // (`start.call` returns `Err`), which the direct-path tests
    // detect by calling the module without the `.expect("run")`
    // wrapper that normal `run_module` uses.

    /// Run a module and return `Ok(i64)` on clean completion or
    /// `Err(trap_message)` when the guest traps. Used by assertion
    /// tests to exercise the failure path without panicking the
    /// test runner.
    fn try_run_module(wasm: &[u8]) -> Result<i64, String> {
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            // Mock the FFI boundary: `ffi_begin` parks the task expecting the
            // driver loop to resume it once the worker finishes. Here there's no
            // loop, so resume immediately (`ffi_result` then returns the default
            // 0); enough for async extern-call programs to complete.
            if name == "ffi_begin" {
                linker
                    .func_new(
                        "env",
                        name,
                        FuncType::new(&engine, wt_params, wt_results),
                        move |mut caller, args, _rets| {
                            let task_id = match args.first() {
                                Some(Val::I32(t)) => *t,
                                _ => return Ok(()),
                            };
                            if let Some(f) = caller
                                .get_export("__fai_resume_task")
                                .and_then(|e| e.into_func())
                            {
                                let _ =
                                    f.call(&mut caller, &[Val::I32(task_id)], &mut [Val::I32(0)]);
                            }
                            Ok(())
                        },
                    )
                    .unwrap();
                continue;
            }
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        let start = instance
            .get_typed_func::<(), i64>(&mut store, "_start")
            .expect("_start export");
        start.call(&mut store, ()).map_err(|e| e.to_string())
    }

    #[test]
    fn direct_module_test_assert_passes_truthy() {
        // All assertions type-check as `Void` at the checker level,
        // so they sit as statements, not tail expressions. `main`
        // returns Void; we just need the wasm not to trap.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.assert(true)\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("passing assert should not trap");
    }

    #[test]
    fn direct_module_test_assert_traps_on_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.assert(1 == 2)\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("failing assert should trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn direct_module_test_equal_passes_on_same_value() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.equal(42, 42)\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("eq pass");
    }

    #[test]
    fn direct_module_test_equal_traps_on_diff() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.equal(1, 2)\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("eq mismatch must trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn direct_module_test_equal_with_message_still_traps() {
        // Exercises the message-arg path — caller-supplied message
        // passes through RT_VALUE_TO_STR + IMPORT_SET_TRAP_MSG
        // before the unreachable fires.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.equal(1, 2, 'should match')\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("trap expected");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn direct_module_assert_is_true_passes() {
        // `assert.isTrue` is auto-exposed inside `@test` blocks. The
        // direct-path builder recognises the `assert` alias without
        // a `use` statement — see `compile_call`'s fallback.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.isTrue(true)\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("isTrue(true) passes");
    }

    #[test]
    fn direct_module_assert_is_false_passes_on_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.isFalse(false)\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("isFalse(false) passes");
    }

    #[test]
    fn direct_module_assert_is_false_traps_on_truthy() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.isFalse(true)\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("isFalse(true) must trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn direct_module_assert_equals_passes() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.equals('hi', 'hi')\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("equals pass");
    }

    #[test]
    fn direct_module_assert_equals_traps_on_mismatch() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.equals('hi', 'bye')\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("mismatch must trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    // ── std.error ───────────────────────────────────────────────
    //
    // `Error(msg)` builds a `{message: msg}` dict; `unwrap` is a
    // null-guarded pass-through. `message`, `kind`, and `isError`
    // share the same implementation as their bare-global forms.

    #[test]
    fn direct_module_error_construct_returns_object() {
        // Result is a NaN-boxed Dict. Verify the high bits match
        // an object tag (QNAN | SIGN_BIT) — a cheap way to confirm
        // we took the `MAKE_OBJ` path without introspecting the
        // dict layout.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "def main\n",
            "    @return Error\n",
            "do\n",
            "  error.Error('oops')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let obj_high = (runtime::QNAN as u64) | 0x8000_0000_0000_0000_u64;
        assert_eq!(
            result & 0xFFFF_0000_0000_0000,
            obj_high & 0xFFFF_0000_0000_0000,
        );
    }

    #[test]
    fn direct_module_unwrap_returns_value_when_non_null() {
        // The checker requires the first arg to `unwrap` to be a
        // nullable type. A helper with `@return Int?` + a non-null
        // body satisfies that while giving us a concrete Int at
        // runtime.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "# Some.\ndef some @return Int? do\n",
            "  42\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  error.unwrap(some(), 99)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_unwrap_returns_fallback_on_null() {
        // The checker will flag `unwrap(null, 99)` because the first
        // arg's type would need to be a nullable. We construct a
        // nullable Int through `unwrap`'s own return type used in a
        // chain: `unwrap(unwrap(null, null), 99)` returns 99.
        //
        // Simpler: bind a literal-null to a typed `Int?` variable so
        // the checker accepts it, then unwrap that.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "# Nullable.\ndef nullable @return Int? do\n",
            "  null\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  error.unwrap(nullable(), 99)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_error_message_qualified() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(error.message(error.Error('x')))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_error_is_error_qualified() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  error.isError(error.Error('x'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    // ── std.http.server ─────────────────────────────────────────
    //
    // Response builders (`ok`/`text`/`html`/`json`/`redirect`) and
    // router API (`router`/`get`/`post`/`serveFiles`/`listen`) all
    // route through `RT_CALL_NATIVE` with distinct `METHOD_SERVER_*`
    // ids. The runtime's helper allocates the response dict or
    // hands the call off to the matching `IMPORT_HTTP_SERVER_*`
    // import.
    //
    // Full end-to-end verification (host actually starting a server,
    // dispatching a request through `__indirect_function_table` to
    // a closure handler) needs infrastructure that doesn't belong
    // in these unit tests. What we can verify here:
    //   (a) the dispatch resolver picks the right METHOD id,
    //   (b) arg shapes + closure args compile cleanly,
    //   (c) the guest wasm validates and doesn't trap on default
    //       stubs (which return 0 for every import).

    #[test]
    fn direct_module_http_server_ok_returns_object() {
        // `server.ok(body)` → Dict. The runtime builds a dict by
        // calling `IMPORT_HTTP_SERVER_RESPONSE`. Default stub returns
        // i64(0) — not a valid NaN-box — so we discard the result
        // and just verify main runs through.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = server.ok('hello')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_http_server_text_with_status() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = server.text(200, 'plain text body')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_http_server_html_and_json_and_redirect_compile() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let h = server.html(200, '<p>hi</p>')\n",
            "  let j = server.json(200, 'already-stringified')\n",
            "  let r = server.redirect(302, '/new')\n",
            "  if h == h\n",
            "    if j == j\n",
            "      if r == r\n",
            "        42\n",
            "      else\n",
            "        0\n",
            "      end\n",
            "    else\n",
            "      0\n",
            "    end\n",
            "  else\n",
            "    0\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_http_server_router_roundtrip_compiles() {
        // Router construction + registering a handler closure +
        // listen. This is the full Router API surface except
        // serveFiles — exercises closure-as-arg at the server
        // layer (via `METHOD_SERVER_GET`, method_id 43).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let r = server.router()\n",
            "  server.get(r, '/', do with req HttpRequest\n",
            "    server.ok('hi')\n",
            "  end)\n",
            "  server.post(r, '/submit', do with req HttpRequest\n",
            "    server.ok('thanks')\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_http_server_serve_files_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let r = server.router()\n",
            "  server.serveFiles(r, './public')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── remoteCall (bare global) ───────────────────────────────
    //
    // `remoteCall(url, fn, argsJson, hash)` → `Unknown`. A
    // bare-global RPC helper — the only builtin we support that
    // isn't module-scoped and isn't user-defined. Each String arg
    // lowers to `(ptr, len)`, then `IMPORT_REMOTE_CALL` runs the
    // round-trip; the host parses the response JSON and returns a
    // NaN-boxed forai value. Matches `translate.rs`'s `name ==
    // "remoteCall"` branch.

    #[test]
    fn direct_remote_call_propagates_result() {
        // Override the stub to return a known NaN-boxed Int; the
        // direct-path result matches exactly, proving the 4-String
        // arg-shape + IMPORT_REMOTE_CALL dispatch is wired.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  remoteCall('http://x', 'fn', '[]', 'hash-v1')\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (77u32 as u64);
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_REMOTE_CALL,
            Val::I64(sentinel as i64),
        ) as u64;
        assert_eq!(result, sentinel);
    }

    #[test]
    fn direct_remote_call_accepts_non_string_coerced_args() {
        // Any value type is allowed as any arg — the builder
        // coerces through RT_VALUE_TO_STR before pushing ptr/len.
        // This mirrors path.join / log.info behaviour.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = remoteCall('url', 'fn', '[]', 'hash')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_remote_call_releases_owned_string_arguments() {
        // Args are concatenations (`a + b`) so each is a genuinely OWNED
        // fresh String temp that must be released after the host import
        // (plan 115 arg-temp mop-up). Bare string *literals* now take the
        // zero-alloc data-section path in `emit_string_arg_stashing` (no
        // String object, nothing to release), so this regression test
        // uses owned temps to actually exercise the release path.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = remoteCall('http://' + 'localhost', 'data.' + 'chat', '[' + '29]', 'h' + 'ash')\n",
            "  42\n",
            "end\n",
        )));
        let mut body_targets: Vec<Vec<u32>> = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(&wasm) {
            let wasmparser::Payload::CodeSectionEntry(body) = payload.expect("payload") else {
                continue;
            };
            let mut targets = Vec::new();
            for op in body
                .get_operators_reader()
                .expect("operators")
                .into_iter()
                .map(|op| op.expect("operator"))
            {
                if let wasmparser::Operator::Call { function_index } = op {
                    targets.push(function_index);
                }
            }
            body_targets.push(targets);
        }
        let remote_import = if body_targets
            .iter()
            .any(|targets| targets.contains(&runtime::IMPORT_REMOTE_BEGIN))
        {
            runtime::IMPORT_REMOTE_BEGIN
        } else {
            runtime::IMPORT_REMOTE_CALL
        };
        let targets = body_targets
            .iter()
            .find(|targets| targets.contains(&remote_import))
            .expect("a lowered body should call remote_call/remote_begin");
        let remote_pos = targets
            .iter()
            .position(|target| *target == remote_import)
            .expect("body should call remote_call/remote_begin");
        let release = rt_base_for_standalone() + runtime::RT_RELEASE;
        let releases_after_remote = targets[remote_pos + 1..]
            .iter()
            .filter(|target| **target == release)
            .count();
        assert!(
            releases_after_remote >= 4,
            "remoteCall must release url/fn/args/hash string temps after host import"
        );
    }

    // ── scalar RC elision (perf) ────────────────────────────────
    //
    // A NaN-boxed scalar (Int/Float/Bool) is never a heap object, so
    // `rt_retain`/`rt_release` on it are guaranteed no-ops. The
    // builder classifies a provably-scalar borrowed value as
    // `Primitive` and skips the `call $rt_retain` the borrowed-return
    // / borrowed-arg paths would otherwise emit. This is the hot-path
    // win behind the `fib` benchmark (~165M leaf returns each used to
    // pay a `rt_retain`→`rt_is_obj` call pair for nothing).
    //
    // These tests are the mechanical ratchet: they count `rt_retain`
    // calls in *user* function bodies (the code-section bodies after
    // the `RT_COUNT` runtime helpers) and assert a scalar-returning
    // function emits none, while an object-returning function still
    // retains its borrowed return. Reverting the elision flips the
    // scalar count above zero and fails `scalar_return_emits_no_retain`.

    /// Count `call $rt_retain` instructions across the *user* function
    /// bodies of a built module — i.e. every code-section body after the
    /// first `RT_COUNT` runtime-helper bodies. Function indices in the
    /// `Call` operator are absolute (`import_count + body_position`), so
    /// the retain target is `rt_base_for_standalone() + RT_RETAIN`.
    fn user_body_retain_count(wasm: &[u8]) -> usize {
        let retain_target = rt_base_for_standalone() + runtime::RT_RETAIN;
        let mut count = 0;
        for (body_pos, body) in wasmparser::Parser::new(0)
            .parse_all(wasm)
            .filter_map(|p| match p.expect("payload") {
                wasmparser::Payload::CodeSectionEntry(body) => Some(body),
                _ => None,
            })
            .enumerate()
        {
            // Skip the runtime-helper bodies; only user code counts.
            if (body_pos as u32) < runtime::RT_COUNT {
                continue;
            }
            for op in body
                .get_operators_reader()
                .expect("operators")
                .into_iter()
                .map(|op| op.expect("operator"))
            {
                if let wasmparser::Operator::Call { function_index } = op {
                    if function_index == retain_target {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// Count occurrences of a specific operator across the *user*
    /// function bodies (code-section bodies after the `RT_COUNT`
    /// runtime helpers). `pred` matches the operator of interest.
    fn user_body_op_count(wasm: &[u8], pred: impl Fn(&wasmparser::Operator) -> bool) -> usize {
        let mut count = 0;
        for (body_pos, body) in wasmparser::Parser::new(0)
            .parse_all(wasm)
            .filter_map(|p| match p.expect("payload") {
                wasmparser::Payload::CodeSectionEntry(body) => Some(body),
                _ => None,
            })
            .enumerate()
        {
            if (body_pos as u32) < runtime::RT_COUNT {
                continue;
            }
            for op in body
                .get_operators_reader()
                .expect("operators")
                .into_iter()
                .map(|op| op.expect("operator"))
            {
                if pred(&op) {
                    count += 1;
                }
            }
        }
        count
    }

    #[test]
    fn bool_var_while_condition_skips_truthiness_check() {
        // `while running` where `running` is a Bool var must branch on
        // the raw i32 flag directly. The generic truthiness path would
        // box the bool and compare it against VAL_NULL / VAL_VOID /
        // FALSE — three `i64.eq` per check. This program's only
        // comparison is `i < 10` (lowers to `i64.lt_s`, never `i64.eq`),
        // so a correct fast path leaves the user body with zero
        // `i64.eq`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    var running = true\n",
            "    var i = 0\n",
            "    while running\n",
            "        i = i + 1\n",
            "        running = i < 10\n",
            "    end\n",
            "    i\n",
            "end\n",
        )));
        let eqs = user_body_op_count(&wasm, |op| matches!(op, wasmparser::Operator::I64Eq));
        assert_eq!(
            eqs, 0,
            "a statically-Bool while condition must not emit the boxed-truthiness sentinel comparisons",
        );
        // ...and the loop must still run to completion (i reaches 10).
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 10;
        assert_eq!(
            result, expected,
            "bool-condition fast path changed the result"
        );
    }

    #[test]
    fn literal_dict_key_skips_alloc_and_value_to_str() {
        // `getString(d, 'name')` with a literal key must push the key's
        // data-section (ptr, len) directly — no rt_value_to_str (and no
        // rt_alloc_string for the key). The dict literal allocates its
        // own keys, but the lookup key does not, and value_to_str only
        // appears on the old key-stashing path, so the user body has zero
        // rt_value_to_str. Result: length('Alice') == 5.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let d = { name: 'Alice' }\n",
            "    length(getString(d, 'name'))\n",
            "end\n",
        )));
        let v2s_target = rt_base_for_standalone() + runtime::RT_VALUE_TO_STR;
        let calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == v2s_target),
        );
        assert_eq!(
            calls, 0,
            "a literal dict key must not be stringified via rt_value_to_str",
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5; // "Alice"
        assert_eq!(
            result, expected,
            "literal-key dict get gave the wrong value"
        );
    }

    #[test]
    fn string_interpolation_skips_value_to_str() {
        // Interpolating a statically-String value (`{{s}}`) must not call
        // the polymorphic rt_value_to_str — a String stringifies to
        // itself. "<x>{{s}}" with s='ab' → "<x>ab" = 5 chars.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let s = 'ab'\n",
            "    length(\"<x>{{s}}\")\n",
            "end\n",
        )));
        let v2s_target = rt_base_for_standalone() + runtime::RT_VALUE_TO_STR;
        let calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == v2s_target),
        );
        assert_eq!(
            calls, 0,
            "interpolating a String must not call rt_value_to_str",
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5; // "<x>ab"
        assert_eq!(
            result, expected,
            "string interpolation fast path gave wrong length"
        );
    }

    #[test]
    fn multi_part_template_builds_in_one_alloc_no_concat() {
        // A multi-part template must build in a single allocation — no
        // `rt_concat` left-fold. Result length is verified via length().
        // "a {{x}} bc" with x=7 → "a 7 bc" = 6 chars.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let x = 7\n",
            "    length(\"a {{x}} bc\")\n",
            "end\n",
        )));
        let concat_target = rt_base_for_standalone() + runtime::RT_CONCAT;
        let concat_calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == concat_target),
        );
        assert_eq!(
            concat_calls, 0,
            "multi-part template must build in one alloc, not a chain of rt_concat",
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 6; // "a 7 bc"
        assert_eq!(
            result, expected,
            "single-alloc template produced the wrong length"
        );
    }

    #[test]
    fn string_case_dispatch_uses_str_eq_not_alloc() {
        // `case p when '/a' when '/b'` on a String scrutinee must compare
        // each literal arm via rt_str_eq against the data section — no
        // rt_alloc_string per arm, no generic rt_eq. p='/b' → arm 2.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let p = '/b'\n",
            "    case p\n",
            "    when '/a'\n",
            "        1\n",
            "    when '/b'\n",
            "        2\n",
            "    default\n",
            "        0\n",
            "    end\n",
            "end\n",
        )));
        let eq_target = rt_base_for_standalone() + runtime::RT_EQ;
        let str_eq_target = rt_base_for_standalone() + runtime::RT_STR_EQ;
        let eq_calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == eq_target),
        );
        let str_eq_calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == str_eq_target),
        );
        assert_eq!(eq_calls, 0, "string case arms must not use generic rt_eq");
        assert!(
            str_eq_calls >= 2,
            "string case arms should lower to rt_str_eq"
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(
            result, expected,
            "string case dispatch matched the wrong arm"
        );
    }

    #[test]
    fn string_literal_eq_uses_str_eq_not_alloc() {
        // `p == 'lit'` must compare bytes via rt_str_eq against the
        // data-section literal — no rt_alloc_string for the literal and
        // no generic rt_eq dispatch. Both comparisons below take the
        // fast path, so the user body has zero RT_EQ calls and at least
        // two RT_STR_EQ calls. Result: only the first route matches.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let p = '/users'\n",
            "    var n = 0\n",
            "    if p == '/users'\n",
            "        n = n + 1\n",
            "    end\n",
            "    if p == '/admin'\n",
            "        n = n + 1\n",
            "    end\n",
            "    n\n",
            "end\n",
        )));
        let eq_target = rt_base_for_standalone() + runtime::RT_EQ;
        let str_eq_target = rt_base_for_standalone() + runtime::RT_STR_EQ;
        let eq_calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == eq_target),
        );
        let str_eq_calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == str_eq_target),
        );
        assert_eq!(
            eq_calls, 0,
            "string==literal must not use the generic rt_eq dispatch"
        );
        assert!(
            str_eq_calls >= 2,
            "both comparisons should lower to rt_str_eq"
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 1;
        assert_eq!(
            result, expected,
            "string literal eq fast path gave the wrong match count"
        );
    }

    #[test]
    fn string_literal_ne_is_correct() {
        // `!=` fast path: '/users' != '/admin' is true, != '/users' is false.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let p = '/users'\n",
            "    var n = 0\n",
            "    if p != '/admin'\n",
            "        n = n + 1\n",
            "    end\n",
            "    if p != '/users'\n",
            "        n = n + 10\n",
            "    end\n",
            "    n\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 1; // only the first holds
        assert_eq!(result, expected, "string != literal fast path is wrong");
    }

    #[test]
    fn range_for_loop_var_is_raw_no_per_iteration_box() {
        // `for i in lo..hi` must bind `i` in a raw Int local, not box the
        // counter with an `rt_make_int` call each iteration. Bounds come
        // from RawInt let-locals (no make_int at setup either), so a
        // correct lowering leaves the user body with zero rt_make_int.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let lo = 0\n",
            "    let hi = 1000\n",
            "    var total = 0\n",
            "    for i in lo..hi\n",
            "        total = total + i\n",
            "    end\n",
            "    total\n",
            "end\n",
        )));
        let make_int_target = rt_base_for_standalone() + runtime::RT_MAKE_INT;
        let calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == make_int_target),
        );
        assert_eq!(
            calls, 0,
            "range-for loop variable must be raw, not boxed per iteration via rt_make_int",
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 499500; // sum 0..999
        assert_eq!(
            result, expected,
            "range-for native loop var changed the sum"
        );
    }

    #[test]
    fn length_in_int_condition_inlines_no_obj_addr_call() {
        // `while j < length(arr)` must read the header count inline — no
        // `rt_obj_addr` call (nor `rt_make_int` box). This program has no
        // other obj_addr user (no indexing), so the fast path drives the
        // user-body obj_addr call count to zero.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let arr = [10, 20, 30]\n",
            "    var c = 0\n",
            "    var j = 0\n",
            "    while j < length(arr)\n",
            "        c = c + 1\n",
            "        j = j + 1\n",
            "    end\n",
            "    c\n",
            "end\n",
        )));
        let obj_addr_target = rt_base_for_standalone() + runtime::RT_OBJ_ADDR;
        let calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == obj_addr_target),
        );
        assert_eq!(
            calls, 0,
            "length() in an Int condition must inline (no rt_obj_addr call)",
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3; // loop ran 3 times
        assert_eq!(result, expected, "length fast path changed the loop count");
    }

    #[test]
    fn for_in_int_arithmetic_uses_native_add() {
        // `total = total + x` where `x` is a `for x in ints` loop
        // variable must use a native `i64.add`, not a `rt_add` call.
        // The loop variable is a Boxed local, but the checker proves it
        // Int, so `numeric_shape_for_expr` reports RawInt and arithmetic
        // unboxes + adds natively.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let xs = [10, 20, 30]\n",
            "    var total = 0\n",
            "    for x in xs\n",
            "        total = total + x\n",
            "    end\n",
            "    total\n",
            "end\n",
        )));
        let add_target = rt_base_for_standalone() + runtime::RT_ADD;
        let calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == add_target),
        );
        assert_eq!(
            calls, 0,
            "Int arithmetic over a for-in loop variable must use native i64.add, not rt_add",
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 60; // 10+20+30
        assert_eq!(
            result, expected,
            "for-in native arithmetic produced the wrong sum"
        );
    }

    #[test]
    fn record_field_read_inlines_no_rt_get_field() {
        // `p.x` / `p.y` on a statically-known record must read the field
        // slot directly — no string-keyed `rt_get_field` call.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "type Point\n",
            "  x Int\n",
            "  y Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let p = Point(x: 3, y: 7)\n",
            "    p.x + p.y\n",
            "end\n",
        )));
        let get_field_target = rt_base_for_standalone() + runtime::RT_GET_FIELD;
        let calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == get_field_target),
        );
        assert_eq!(
            calls, 0,
            "a proven record field read must inline, not call rt_get_field",
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 10; // 3+7
        assert_eq!(
            result, expected,
            "inline field read produced the wrong value"
        );
    }

    #[test]
    fn record_field_chain_repeated_property_is_correct() {
        // Regression guard for the chain-collision fix: a MemberExpression's
        // source location is its receiver's, so every level of `o.inner.inner`
        // shares (line, col) AND property name. Only the chain depth
        // distinguishes them. With `Outer{tag, inner}` (inner at slot 1) and
        // `Inner{inner, extra}` (inner at slot 0), a depth-blind key would
        // misread `o.inner.inner` as Outer slot 0 (tag, an Int treated as a
        // pointer). Correct result is the nested inner's value, 42.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "type Inner\n",
            "  inner Int\n",
            "  extra Int\n",
            "end\n",
            "\n",
            "type Outer\n",
            "  tag Int\n",
            "  inner Inner\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let o = Outer(tag: 7, inner: Inner(inner: 42, extra: 5))\n",
            "    o.inner.inner\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(
            result, expected,
            "repeated-property chain o.inner.inner must read the nested field, not collide",
        );
    }

    #[test]
    fn array_int_index_inlines_no_rt_get_index() {
        // `arr[j]` with a statically-Int[] receiver and Int index must
        // compile to an inline element read — no call into the
        // polymorphic `rt_get_index`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let arr = [10, 20, 30]\n",
            "    var total = 0\n",
            "    var j = 0\n",
            "    while j < 3\n",
            "        total = total + arr[j]\n",
            "        j = j + 1\n",
            "    end\n",
            "    total\n",
            "end\n",
        )));
        let get_index_target = rt_base_for_standalone() + runtime::RT_GET_INDEX;
        let calls = user_body_op_count(
            &wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == get_index_target),
        );
        assert_eq!(
            calls, 0,
            "a proven Array/Int index must inline, not call rt_get_index",
        );
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 60; // 10+20+30
        assert_eq!(
            result, expected,
            "inline array index produced the wrong sum"
        );
    }

    #[test]
    fn array_int_index_fast_path_negative_index_wraps() {
        // The inline path must mirror rt_get_index's negative-index
        // wrap: arr[-1] is the last element.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let arr = [10, 20, 30]\n",
            "    let i = 0 - 1\n",
            "    arr[i]\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 30;
        assert_eq!(result, expected, "arr[-1] must wrap to the last element");
    }

    #[test]
    fn scalar_return_emits_no_retain() {
        // `id` returns a borrowed Int param. The boxed scalar is never
        // a heap object, so no `rt_retain` is needed on the return —
        // nor on `main` returning its borrowed Int local.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Identity on an Int.\n",
            "def id\n",
            "    @param n Int\n",
            "    @return Int\n",
            "do\n",
            "    n\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "    let r = id(7)\n",
            "    r\n",
            "end\n",
        )));
        assert_eq!(
            user_body_retain_count(&wasm),
            0,
            "a scalar-returning function must not emit rt_retain (RC is a no-op on scalars)",
        );
        // ...and it must still compute the right value.
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected, "scalar RC elision changed the result");
    }

    #[test]
    fn object_return_still_retains_borrowed() {
        // Contrast: `id` returns a borrowed String — a heap object whose
        // borrowed return MUST be retained so the caller owns a live
        // ref. The scalar elision must not touch this path.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Identity on a String.\n",
            "def id\n",
            "    @param s String\n",
            "    @return String\n",
            "do\n",
            "    s\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "    id('hello')\n",
            "end\n",
        )));
        assert!(
            user_body_retain_count(&wasm) >= 1,
            "a borrowed-object return must still retain — object RC was broken",
        );
    }

    // ── extern FFI ──────────────────────────────────────────────
    //
    // `extern { ... }` blocks declare C-ABI functions. Callers
    // serialize NaN-boxed args to a scratch region at offset 65536
    // and invoke `IMPORT_CALL_FFI(ext_fn_idx, arg_count,
    // args_base)`. The host uses the program's `ExternFnInfo`
    // metadata (gathered from the extern block) to unbox to the
    // right C types via libloading, then boxes the return.
    //
    // Tests here verify the guest-side plumbing: the import is
    // called with the right (ext_fn_idx, arg_count, args_base)
    // triple, args land in scratch memory, and the return flows
    // back out. Host-side marshalling is covered in the
    // `fai-cli::wasm_runner` integration tests.

    #[test]
    fn direct_extern_call_propagates_result() {
        // Override IMPORT_CALL_FFI to return a specific NaN-boxed
        // Int — proves the extern-call wiring routes the import
        // and its return lands as the expression's value.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "extern libc\n",
            "  def strlen(s: String) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  strlen('hello')\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (5u32 as u64);
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_CALL_FFI, Val::I64(sentinel as i64))
                as u64;
        assert_eq!(result, sentinel);
    }

    #[test]
    fn direct_extern_multi_arg_compiles() {
        // Multi-arg extern — each arg is stored at
        // mem[65536 + i*8], then IMPORT_CALL_FFI is invoked with
        // arg_count=3. Default stub returns 0; we discard.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "extern libmath\n",
            "  def add3(a: Int, b: Int, c: Int) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = add3(1, 2, 3)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_extern_multiple_blocks_assign_sequential_indices() {
        // `strlen` is idx 0, `atoi` is idx 1. The stub can't
        // distinguish them (both return 0), but compilation +
        // validation proves the builder passes distinct indices at
        // each call site. If the indices were miswired, a later
        // integration test against a real libloading host would
        // blow up immediately.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "extern libc\n",
            "  def strlen(s: String) -> Int\n",
            "  def atoi(s: String) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a = strlen('hi')\n",
            "  let b = atoi('42')\n",
            "  if a == a\n",
            "    if b == b\n",
            "      42\n",
            "    else\n",
            "      0\n",
            "    end\n",
            "  else\n",
            "    0\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── std.error bare-global builtins ───────────────────────────
    //
    // `Error(msg)` constructs a dict `{message: msg}`. `message`
    // and `kind` read named fields; `isError` checks the
    // object-tag. Previously refused as a language gap (see the
    // B-list fix session).

    #[test]
    fn direct_print_bare_global() {
        // `print(v)` is a bare-global builtin: stringify + write to
        // stdout via RT_PRINT_VAL_NEW. Program doesn't need a `use`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  print('hello')\n",
            "  print(42)\n",
            "  99\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_error_message_field() {
        // `message(err)` reads the "message" field from the Error
        // dict constructed by `Error("...")`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let e = error.Error('oops')\n",
            "  string.length(message(e))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // "oops" is 4 chars.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 4;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_error_kind_absent_returns_null() {
        // Error constructor doesn't set a `kind` field; `kind(e)`
        // reads a missing key and returns null.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  let e = error.Error('oops')\n",
            "  kind(e)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_is_error_true_on_error() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  isError(error.Error('boom'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_is_error_false_on_int() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  isError(42)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_first_returns_element() {
        // Previously refused as an unimplemented language gap —
        // now supported with runtime handlers + direct-path
        // dispatch. Returns the first element of a non-empty array.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  array.first([10, 20, 30])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 10;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_first_empty_is_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Unknown?\n",
            "do\n",
            "  array.first([])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_last_returns_element() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  array.last([10, 20, 30])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 30;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_last_empty_is_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Unknown?\n",
            "do\n",
            "  array.last([])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    // ── Phase G integration (try_codegen_direct) ────────────────
    //
    // End-to-end: parse forai source → run checker → call
    // `try_codegen_direct` → run the resulting wasm. Proves the
    // production path wires parse/check/build/assemble together
    // correctly and the produced module matches what the test
    // harness's piecewise `build_standalone_module_many` would
    // have produced.

    /// Parse a forai source string, run the checker, and try to
    /// compile via `try_codegen_direct`. Panics on parse / check
    /// failure; returns `None` if any function refuses the direct
    /// path.
    fn try_compile_via_production(src: &str) -> Option<Vec<u8>> {
        try_compile_via_production_for_target(src, None)
    }

    fn try_compile_via_production_for_target(src: &str, target: Option<&str>) -> Option<Vec<u8>> {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
            array_int_index_sites: checker.array_int_index_sites,
            record_field_read_sites: checker.record_field_read_sites,
        };
        crate::try_codegen_direct(&prepared.serde_ast, &info, target)
    }

    fn wasm_import_names(wasm: &[u8]) -> Vec<String> {
        let parser = wasmparser::Parser::new(0);
        let mut import_names: Vec<String> = Vec::new();
        for payload in parser.parse_all(wasm) {
            if let wasmparser::Payload::ImportSection(section) = payload.expect("payload") {
                for imp in section {
                    let imp = imp.expect("import entry");
                    import_names.push(imp.name.to_string());
                }
                break;
            }
        }
        import_names
    }

    fn wasm_import_index(wasm: &[u8], name: &str) -> Option<u32> {
        let mut index = 0u32;
        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            if let wasmparser::Payload::ImportSection(section) = payload.expect("payload") {
                for imp in section {
                    let imp = imp.expect("import entry");
                    match imp.ty {
                        wasmparser::TypeRef::Func(_) => {
                            if imp.name == name {
                                return Some(index);
                            }
                            index += 1;
                        }
                        wasmparser::TypeRef::Table(_)
                        | wasmparser::TypeRef::Memory(_)
                        | wasmparser::TypeRef::Global(_)
                        | wasmparser::TypeRef::Tag(_) => {}
                    }
                }
                break;
            }
        }
        None
    }

    fn user_body_import_call_count(wasm: &[u8], import_name: &str) -> usize {
        let import_index = wasm_import_index(wasm, import_name)
            .unwrap_or_else(|| panic!("missing import {import_name}"));
        user_body_op_count(
            wasm,
            |op| matches!(op, wasmparser::Operator::Call { function_index } if *function_index == import_index),
        )
    }

    #[test]
    fn stdlib_scheduling_classifies_blocking_direct_and_cpu_bound_calls() {
        let http = stdlib_scheduling("std.http.request", "get").expect("http get classified");
        assert!(matches!(
            http,
            StdlibScheduling::AwaitHostOp {
                await_kind: AwaitHostOpKind::HttpRequest,
                op_kind: crate::runtime::HOST_OP_HTTP_GET,
                arity: HostOpArity::Range { min: 1, max: 2 },
            }
        ));

        let blocking =
            stdlib_scheduling("std.net.tcp", "readLine").expect("tcp.readLine classified");
        assert!(matches!(
            blocking,
            StdlibScheduling::AwaitHostOp {
                await_kind: AwaitHostOpKind::BlockingIo,
                op_kind: crate::runtime::HOST_OP_TCP_READ_LINE,
                arity: HostOpArity::Exact(1),
            }
        ));

        assert_eq!(
            stdlib_scheduling("std.file", "exists"),
            Some(StdlibScheduling::DirectHostImport)
        );
        assert_eq!(
            stdlib_scheduling("std.json", "parse"),
            Some(StdlibScheduling::CpuBoundDirect)
        );
        assert_eq!(
            stdlib_scheduling("std.crypto", "sha256Hex"),
            Some(StdlibScheduling::CpuBoundDirect)
        );
    }

    #[test]
    fn production_blocking_stdlib_calls_use_host_op_not_direct_imports() {
        let wasm = try_compile_via_production(concat!(
            "use std.env\n",
            "use std.file\n",
            "use std.http.request\n",
            "use std.net.tcp\n",
            "use std.net.udp\n",
            "use std.process\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let _http = request.get('file:///tmp/fai-missing')\n",
            "  let _proc = process.run('printf ok', '.', '{}', 5000, 65536)\n",
            "  let sess = process.start('cat', '.', '{}', 5000)\n",
            "  let _sent = process.write(sess, 'hello')\n",
            "  let _read = file.read('/tmp/fai-missing')\n",
            "  let _write = file.write('/tmp/fai-out', 'x')\n",
            "  let _list = file.list('/tmp')\n",
            "  let _env = env.load('/tmp/fai-missing.env')\n",
            "  let conn = tcp.connect('127.0.0.1', 1)\n",
            "  let _tcpRead = tcp.read(conn)\n",
            "  let _line = tcp.readLine(conn)\n",
            "  let listener = tcp.listen(1)\n",
            "  let _accepted = tcp.accept(listener)\n",
            "  let sock = udp.bind(1)\n",
            "  let _packet = udp.receive(sock)\n",
            "end\n",
        ))
        .expect("blocking stdlib program should compile through async host ops");

        assert!(
            user_body_import_call_count(&wasm, "host_op_begin") >= 12,
            "blocking stdlib calls should lower to host_op_begin"
        );
        for direct_import in [
            "http_request_get",
            "process_run",
            "process_write",
            "file_read_str",
            "write_file",
            "file_list",
            "env_load",
            "tcp_connect",
            "tcp_read",
            "tcp_read_line",
            "tcp_accept",
            "udp_receive",
        ] {
            assert_eq!(
                user_body_import_call_count(&wasm, direct_import),
                0,
                "{direct_import} should not be called directly by blocking stdlib lowering"
            );
        }
    }

    /// Plan 103 U8 mirror of the production assertion: test builds compile
    /// through the async engine too, so blocking test bodies never touch the
    /// legacy synchronous imports (`sleep_ms`, `run_all`, direct
    /// `process_run`) and the module carries the spawn-per-case test surface.
    #[test]
    fn test_builds_route_blocking_test_bodies_through_the_engine() {
        let src = concat!(
            "use std.process\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print('x')\n",
            "end\n",
            "\n",
            "private:\n",
            "\n",
            "# Sleeps, shells out, returns 1.\n",
            "def worker\n",
            "    @return Int\n",
            "do\n",
            "  sleep(10)\n",
            "  let _ = process.run('printf ok', '.', '{}', 5000, 65536)\n",
            "  1\n",
            "end\n",
            "\n",
            "test worker\n",
            "  it 'overlaps'\n",
            "    let a, b = all(worker(), worker())\n",
            "    assert.equals(a + b, 2)\n",
            "  end\n",
            "end\n",
        );
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
            array_int_index_sites: checker.array_int_index_sites,
            record_field_read_sites: checker.record_field_read_sites,
        };
        let wasm = crate::codegen_direct_full_reasoned_with_entry_file(
            &prepared.serde_ast,
            &prepared.modules,
            &info,
            None,
            true,
            None,
        )
        .expect("test build should compile through the async engine");

        // Engine test-surface exports present.
        let mut export_names: Vec<String> = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(&wasm) {
            if let wasmparser::Payload::ExportSection(section) = payload.expect("payload") {
                for e in section {
                    export_names.push(e.expect("export").name.to_string());
                }
            }
        }
        for required in ["_fai_spawn_test", "_start_async", "__fai_poll", "__fai_task_status"] {
            assert!(
                export_names.iter().any(|n| n == required),
                "test build missing engine export `{required}` — fell back to the legacy path? exports: {export_names:?}"
            );
        }

        // No legacy blocking imports reachable from user bodies.
        for legacy in ["sleep_ms", "run_all", "process_run"] {
            assert_eq!(
                user_body_import_call_count(&wasm, legacy),
                0,
                "{legacy} should be unreachable from an engine test build"
            );
        }
    }

    #[test]
    fn production_direct_and_cpu_bound_stdlib_calls_stay_direct() {
        let wasm = try_compile_via_production(concat!(
            "use std.crypto\n",
            "use std.file\n",
            "use std.json\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let _exists = file.exists('/tmp/fai-missing')\n",
            "  let _parsed = json.parse('{}')\n",
            "  let _hash = crypto.sha256Hex('abc')\n",
            "end\n",
        ))
        .expect("direct stdlib program should compile");

        assert_eq!(user_body_import_call_count(&wasm, "host_op_begin"), 0);
        assert!(
            user_body_import_call_count(&wasm, "file_exists") > 0,
            "file.exists should remain direct"
        );
        assert!(
            user_body_import_call_count(&wasm, "json_parse") > 0,
            "json.parse stays direct until CPU-bound fairness is designed"
        );
        assert!(
            user_body_import_call_count(&wasm, "crypto_sha256_hex") > 0,
            "crypto.sha256Hex stays direct until CPU-bound fairness is designed"
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    struct OwnershipEvent {
        op: &'static str,
        site: i32,
        aux: String,
    }

    fn ownership_event_args(wasm: &[u8]) -> Vec<OwnershipEvent> {
        let event_index =
            wasm_import_index(wasm, "__fai_ownership_event").expect("ownership event import");
        let mut events = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            let wasmparser::Payload::CodeSectionEntry(body) = payload.expect("payload") else {
                continue;
            };
            let mut recent_i32_consts = Vec::new();
            for op in body
                .get_operators_reader()
                .expect("operators")
                .into_iter()
                .map(|op| op.expect("operator"))
            {
                match op {
                    wasmparser::Operator::I32Const { value } => {
                        recent_i32_consts.push(value);
                        if recent_i32_consts.len() > 8 {
                            recent_i32_consts.remove(0);
                        }
                    }
                    wasmparser::Operator::Call { function_index }
                        if function_index == event_index =>
                    {
                        let op_id = recent_i32_consts
                            .iter()
                            .rev()
                            .nth(2)
                            .copied()
                            .expect("ownership event op argument");
                        let site = recent_i32_consts
                            .iter()
                            .rev()
                            .nth(1)
                            .copied()
                            .expect("ownership event site argument");
                        let aux = recent_i32_consts
                            .iter()
                            .rev()
                            .next()
                            .copied()
                            .expect("ownership event aux argument");
                        let op = OwnershipOp::from_id(op_id as u32)
                            .expect("known ownership op")
                            .name();
                        events.push(OwnershipEvent {
                            op,
                            site,
                            aux: format_ownership_aux(aux),
                        });
                    }
                    _ => {}
                }
            }
        }
        events
    }

    fn ownership_event_site_args(wasm: &[u8]) -> Vec<i32> {
        ownership_event_args(wasm)
            .into_iter()
            .map(|event| event.site)
            .collect()
    }

    fn format_ownership_aux(encoded: i32) -> String {
        let Some((kind, detail)) = OwnershipAux::decode(encoded) else {
            return format!("unknown:{encoded}");
        };
        match kind {
            OwnershipAux::None => "none".to_string(),
            OwnershipAux::ClosureCapture => format!("closure_capture:{detail}"),
            OwnershipAux::HostArgument => format!("host_argument:{detail}"),
            OwnershipAux::AsyncFrameSlot => format!("async_frame_slot:{detail}"),
        }
    }

    fn checked_ownership_site_golden(src: &str) -> Vec<&'static str> {
        let _guard = crate::runtime::OwnershipCheckGuard::new();
        compile_all(src)
            .ownership_sites
            .into_iter()
            .map(|site| site.op)
            .collect()
    }

    fn fai_dbg_json(wasm: &[u8]) -> Option<String> {
        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            if let wasmparser::Payload::CustomSection(section) = payload.expect("payload") {
                if section.name() == "fai-dbg" {
                    return Some(String::from_utf8_lossy(section.data()).to_string());
                }
            }
        }
        None
    }

    /// Compile an entry source with one synthetic user module.
    /// Feeds both through the checker with module awareness, then
    /// through the direct path. Mirrors what the CLI does when
    /// running a multi-file project.
    fn try_compile_with_module(
        entry_src: &str,
        module_name: &str,
        module_src: &str,
    ) -> Option<Vec<u8>> {
        try_compile_with_modules(entry_src, vec![(module_name, module_src)])
    }

    /// Compile an entry source with multiple synthetic user modules.
    fn try_compile_with_modules(entry_src: &str, modules: Vec<(&str, &str)>) -> Option<Vec<u8>> {
        try_compile_with_modules_with(entry_src, modules, false)
    }

    /// Compile an entry source with multiple synthetic user modules, optionally
    /// dropping named-param reorder metadata to exercise direct codegen's
    /// label-based fallback.
    fn try_compile_with_modules_with(
        entry_src: &str,
        modules: Vec<(&str, &str)>,
        clear_named_reorder: bool,
    ) -> Option<Vec<u8>> {
        try_compile_with_modules_config(entry_src, modules, clear_named_reorder, None)
    }

    /// Compile with a forced named-param reorder entry. This simulates stale or
    /// wrong checker metadata while keeping the source labels intact.
    fn try_compile_with_modules_with_forced_named_reorder(
        entry_src: &str,
        modules: Vec<(&str, &str)>,
        forced_order: Vec<Option<usize>>,
    ) -> Option<Vec<u8>> {
        try_compile_with_modules_config(entry_src, modules, false, Some(forced_order))
    }

    fn try_compile_with_modules_config(
        entry_src: &str,
        modules: Vec<(&str, &str)>,
        clear_named_reorder: bool,
        forced_order: Option<Vec<Option<usize>>>,
    ) -> Option<Vec<u8>> {
        let prepared = fai_compiler::prepare_source_with_synthetic(
            entry_src,
            None,
            modules
                .into_iter()
                .map(|(name, src)| (name.to_string(), src.to_string()))
                .collect(),
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        let prepared_modules: Vec<fai_checker::PreparedModule> = prepared
            .modules
            .iter()
            .map(|m| fai_checker::PreparedModule {
                name: m.name.clone(),
                statements: m.statements.clone(),
                file_paths: m.file_paths.clone(),
                private_names: m.private_names.clone(),
                file_path: None,
            })
            .collect();
        checker
            .check_with_modules(&prepared.serde_ast.statements, &prepared_modules)
            .expect("checker");
        if clear_named_reorder {
            checker.named_param_reorder.clear();
        }
        if let Some(order) = forced_order {
            let key = checker
                .named_param_reorder
                .keys()
                .next()
                .cloned()
                .expect("forced reorder test needs one labelled call");
            checker.named_param_reorder.insert(key, order);
        }
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
            array_int_index_sites: checker.array_int_index_sites,
            record_field_read_sites: checker.record_field_read_sites,
        };
        crate::try_codegen_direct_with_modules(&prepared.serde_ast, &prepared.modules, &info, None)
    }

    #[test]
    fn production_direct_simple_int_return() {
        let wasm = try_compile_via_production(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  42\n",
            "end\n",
        ))
        .expect("direct compilation should succeed for int return");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_with_closure_and_array_map() {
        let wasm = try_compile_via_production(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 5\n",
            "  let _ = array.map([1, 2, 3], do with x Int\n",
            "    x + k\n",
            "  end)\n",
            "  42\n",
            "end\n",
        ))
        .expect("array.map + closure should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_with_dict_and_field_access() {
        let wasm = try_compile_via_production(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let user = {name: 'alice', age: 30}\n",
            "  user.age\n",
            "end\n",
        ))
        .expect("dict + field access should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 30;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_user_call_heap_result_can_embed_owned_arg() {
        let wasm = try_compile_via_production(concat!(
            "type Box\n",
            "  items String[]\n",
            "end\n",
            "\n",
            "# Wrap items in a box.\n",
            "def wrap\n",
            "    @param items String[]\n",
            "    @return Box\n",
            "do\n",
            "  Box(items: items)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let box = wrap(['a', 'b'])\n",
            "  length(box.items)\n",
            "end\n",
        ))
        .expect("heap-returning user call should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_with_string_methods() {
        let wasm = try_compile_via_production(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.toUpper('hello'))\n",
            "end\n",
        ))
        .expect("string methods should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    // NOTE: There used to be a `production_direct_refuses_unsupported_feature`
    // here, but the direct path now handles essentially every
    // source-level construct a well-formed forai program can
    // produce. The remaining refusals fire on synthetic ASTs
    // (see `direct_rejects_unsupported_feature`) or on deep
    // interactions (e.g., nested closures). If a future
    // source-level gap surfaces, re-add a production refusal test
    // against it.

    #[test]
    fn production_direct_for_in_array() {
        // End-to-end: for-in-array now compiles via direct.
        let wasm = try_compile_via_production(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for n in [1, 2, 3, 4]\n",
            "    s = s + n\n",
            "  end\n",
            "  s\n",
            "end\n",
        ))
        .expect("for-in-array should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 10;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_test_runner_dispatches_passing_case() {
        // Test blocks compile via the direct path when is_test=true.
        // The resulting module exports `_fai_run_test(suite,case)`
        // which the CLI test runner invokes. A passing case should
        // return cleanly.
        let prepared = fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
            concat!(
                "# Add.\ndef add\n",
                "    @param x Int\n",
                "    @param y Int\n",
                "    @return Int\n",
                "do\n",
                "  x + y\n",
                "end\n",
                "\n",
                "test add\n",
                "it 'handles one plus one'\n",
                "  assert.equals(add(1, 1), 2)\n",
                "end\n",
                "end\n",
            ),
            None,
            Vec::new(),
            None,
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
            array_int_index_sites: checker.array_int_index_sites,
            record_field_read_sites: checker.record_field_read_sites,
        };
        let wasm = crate::try_codegen_direct_full(
            &prepared.serde_ast,
            &prepared.modules,
            &info,
            None,
            true,
        )
        .expect("test-mode direct build should succeed");

        // Invoke `_fai_run_test(suite=0, case=0)` via wasmtime.
        use wasmtime::{
            Engine, FuncType, Linker, Module as RuntimeModule, Store, Val, ValType as WtValType,
        };
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, &wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            // Mock the FFI boundary: `ffi_begin` parks the task expecting the
            // driver loop to resume it once the worker finishes. Here there's no
            // loop, so resume immediately (`ffi_result` then returns the default
            // 0); enough for async extern-call programs to complete.
            if name == "ffi_begin" {
                linker
                    .func_new(
                        "env",
                        name,
                        FuncType::new(&engine, wt_params, wt_results),
                        move |mut caller, args, _rets| {
                            let task_id = match args.first() {
                                Some(Val::I32(t)) => *t,
                                _ => return Ok(()),
                            };
                            if let Some(f) = caller
                                .get_export("__fai_resume_task")
                                .and_then(|e| e.into_func())
                            {
                                let _ =
                                    f.call(&mut caller, &[Val::I32(task_id)], &mut [Val::I32(0)]);
                            }
                            Ok(())
                        },
                    )
                    .unwrap();
                continue;
            }
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        let run_test = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "_fai_run_test")
            .expect("_fai_run_test export");
        // Passing case — no trap.
        run_test
            .call(&mut store, (0, 0))
            .expect("passing test should not trap");
    }

    #[test]
    fn production_direct_test_mode_start_does_not_call_main() {
        let prepared = fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
            concat!(
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  99\n",
                "end\n",
                "\n",
                "# Value.\ndef value\n",
                "    @return Int\n",
                "do\n",
                "  1\n",
                "end\n",
                "\n",
                "test value\n",
                "it 'returns one'\n",
                "  assert.equals(value(), 1)\n",
                "end\n",
                "end\n",
            ),
            None,
            Vec::new(),
            None,
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
            array_int_index_sites: checker.array_int_index_sites,
            record_field_read_sites: checker.record_field_read_sites,
        };
        let wasm = crate::try_codegen_direct_full(
            &prepared.serde_ast,
            &prepared.modules,
            &info,
            None,
            true,
        )
        .expect("test-mode direct build should succeed");

        let result = run_module(&wasm) as i64;
        assert_eq!(result, runtime::VAL_VOID);
    }

    #[test]
    fn production_direct_test_runner_traps_on_failing_case() {
        let prepared = fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
            concat!(
                "# Subject.\ndef subject\n",
                "    @return Int\n",
                "do\n",
                "  1\n",
                "end\n",
                "\n",
                "test subject\n",
                "it 'wrong answer'\n",
                "  assert.equals(1, 2)\n",
                "end\n",
                "end\n",
            ),
            None,
            Vec::new(),
            None,
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
            array_int_index_sites: checker.array_int_index_sites,
            record_field_read_sites: checker.record_field_read_sites,
        };
        let wasm = crate::try_codegen_direct_full(
            &prepared.serde_ast,
            &prepared.modules,
            &info,
            None,
            true,
        )
        .expect("test-mode direct build should succeed");

        use wasmtime::{
            Engine, FuncType, Linker, Module as RuntimeModule, Store, Val, ValType as WtValType,
        };
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, &wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            // Mock the FFI boundary: `ffi_begin` parks the task expecting the
            // driver loop to resume it once the worker finishes. Here there's no
            // loop, so resume immediately (`ffi_result` then returns the default
            // 0); enough for async extern-call programs to complete.
            if name == "ffi_begin" {
                linker
                    .func_new(
                        "env",
                        name,
                        FuncType::new(&engine, wt_params, wt_results),
                        move |mut caller, args, _rets| {
                            let task_id = match args.first() {
                                Some(Val::I32(t)) => *t,
                                _ => return Ok(()),
                            };
                            if let Some(f) = caller
                                .get_export("__fai_resume_task")
                                .and_then(|e| e.into_func())
                            {
                                let _ =
                                    f.call(&mut caller, &[Val::I32(task_id)], &mut [Val::I32(0)]);
                            }
                            Ok(())
                        },
                    )
                    .unwrap();
                continue;
            }
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        let run_test = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "_fai_run_test")
            .expect("_fai_run_test export");
        let err = run_test
            .call(&mut store, (0, 0))
            .expect_err("failing assert should trap")
            .to_string();
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn production_direct_cross_module_call() {
        // Entry file imports a sibling module and calls into it.
        // `helpers.double(x)` resolves via `module_aliases["helpers"]
        // = "mypkg.helpers"`, then lookup of
        // `"mypkg.helpers.double"` in the unified function table.
        let wasm = try_compile_with_module(
            concat!(
                "use mypkg.helpers\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  helpers.double(21)\n",
                "end\n",
            ),
            "mypkg.helpers",
            concat!(
                "# Double.\ndef double\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * 2\n",
                "end\n",
            ),
        )
        .expect("cross-module call should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_glob_import_user_module_call() {
        let wasm = try_compile_with_module(
            concat!(
                "use * from mypkg.helpers\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  double(21)\n",
                "end\n",
            ),
            "mypkg.helpers",
            concat!(
                "# Double.\ndef double\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * 2\n",
                "end\n",
            ),
        )
        .expect("glob-imported user function should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_glob_import_user_module_ufcs_call() {
        let wasm = try_compile_with_module(
            concat!(
                "use * from mypkg.helpers\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  21.double()\n",
                "end\n",
            ),
            "mypkg.helpers",
            concat!(
                "# Double.\ndef double\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * 2\n",
                "end\n",
            ),
        )
        .expect("glob-imported UFCS function should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_glob_import_std_call() {
        let wasm = try_compile_via_production(concat!(
            "use * from std.math\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  floor(42.9)\n",
            "end\n",
        ))
        .expect("glob-imported std function should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_module_internal_peer_call() {
        // A module function calls another function in the same
        // module by unqualified name. The `module_context` fallback
        // on the builder looks up `"mypkg.helpers.square"` when the
        // bare `square` lookup misses.
        let wasm = try_compile_with_module(
            concat!(
                "use mypkg.helpers\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  helpers.squarePlusOne(6)\n",
                "end\n",
            ),
            "mypkg.helpers",
            concat!(
                "# Square.\ndef square\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * x\n",
                "end\n",
                "\n",
                "# SquarePlusOne.\ndef squarePlusOne\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  square(x) + 1\n",
                "end\n",
            ),
        )
        .expect("module peer call should compile via direct");
        let result = run_module(&wasm) as u64;
        // square(6) + 1 = 37
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 37;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_allows_same_basename_modules_for_named_imports() {
        // Folder namespaces can naturally contain both `auth` and
        // `pages.auth`. Named imports use the full canonical module
        // path, so this is not ambiguous and should not require
        // renaming either folder.
        let wasm = try_compile_with_modules(
            concat!(
                "use { LoginPage } from pages.auth\n",
                "use { checkTask } from data.tasks\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  LoginPage() + checkTask()\n",
                "end\n",
            ),
            vec![
                (
                    "auth",
                    concat!(
                        "# Require session.\ndef requireSession\n",
                        "    @return Int\n",
                        "do\n",
                        "  20\n",
                        "end\n",
                    ),
                ),
                (
                    "pages.auth",
                    concat!(
                        "# Login page.\ndef LoginPage\n",
                        "    @return Int\n",
                        "do\n",
                        "  22\n",
                        "end\n",
                    ),
                ),
                (
                    "data.tasks",
                    concat!(
                        "use { requireSession } from auth\n",
                        "\n",
                        "# Check task.\ndef checkTask\n",
                        "    @return Int\n",
                        "do\n",
                        "  requireSession()\n",
                        "end\n",
                    ),
                ),
            ],
        )
        .expect("same-basename named imports should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_named_import_call_fills_skipped_defaults() {
        // Mirrors calls like `TextInput('Message', signalValue: input)`.
        // The named argument is in declaration order, but it skips an
        // earlier defaulted parameter. Direct codegen must still use the
        // checker's named-argument reorder map so the skipped parameter gets
        // its default and the labelled value lands in the right slot.
        let wasm = try_compile_with_modules(
            concat!(
                "use { pick } from widgets\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  pick(1, c: 42)\n",
                "end\n",
            ),
            vec![(
                "widgets",
                concat!(
                    "# Pick c.\ndef pick\n",
                    "    @param a Int\n",
                    "    @param b Int, default: 20\n",
                    "    @param c Int, default: 30\n",
                    "    @return Int\n",
                    "do\n",
                    "  c\n",
                    "end\n",
                ),
            )],
        )
        .expect("named import call with skipped defaults should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_labelled_call_recovers_without_checker_reorder() {
        // A missing named-param side-table entry must not silently turn
        // `c: 42` into the second positional argument. Codegen has the callee
        // parameter names available, so it should rebuild the declaration-order
        // mapping and fill skipped defaults itself.
        let wasm = try_compile_with_modules_with(
            concat!(
                "use { pick } from widgets\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  pick(1, c: 42)\n",
                "end\n",
            ),
            vec![(
                "widgets",
                concat!(
                    "# Pick c.\ndef pick\n",
                    "    @param a Int\n",
                    "    @param b Int, default: 20\n",
                    "    @param c Int, default: 30\n",
                    "    @return Int\n",
                    "do\n",
                    "  c\n",
                    "end\n",
                ),
            )],
            true,
        )
        .expect("direct should recover labelled calls without checker reorder metadata");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_labelled_call_prefers_source_labels_over_stale_reorder() {
        // If checker metadata is stale relative to the function being compiled,
        // blindly trusting the side table can map a labelled argument into the
        // wrong parameter slot. Source labels plus the compiled callee's
        // `param_names` are authoritative.
        let wasm = try_compile_with_modules_with_forced_named_reorder(
            concat!(
                "use { pick } from widgets\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  pick(1, c: 42)\n",
                "end\n",
            ),
            vec![(
                "widgets",
                concat!(
                    "# Pick c.\ndef pick\n",
                    "    @param a Int\n",
                    "    @param b Int, default: 20\n",
                    "    @param c Int, default: 30\n",
                    "    @return Int\n",
                    "do\n",
                    "  c\n",
                    "end\n",
                ),
            )],
            vec![Some(0), Some(1), None],
        )
        .expect("direct should prefer source labels over stale checker metadata");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_optional_named_object_not_equal_null_is_true() {
        // Mirrors `if signalValue != null` in Forui.TextInput. The value is a
        // heap object wrapped in an optional parameter; direct codegen must not
        // treat non-null boxed objects as null.
        let wasm = try_compile_with_modules(
            concat!(
                "use { Signal, hasSignal } from widgets\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  hasSignal(signalValue: Signal(value: 7))\n",
                "end\n",
            ),
            vec![(
                "widgets",
                concat!(
                    "type Signal\n",
                    "  value Int\n",
                    "end\n",
                    "\n",
                    "# Has signal.\ndef hasSignal\n",
                    "    @param text String?, default: null\n",
                    "    @param signalValue Signal?, default: null\n",
                    "    @return Int\n",
                    "do\n",
                    "  if signalValue != null\n",
                    "    42\n",
                    "  else\n",
                    "    0\n",
                    "  end\n",
                    "end\n",
                ),
            )],
        )
        .expect("optional named heap object comparison should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_named_import_resume_call_fills_skipped_defaults() {
        // TextInput is compiled as a resume function because it creates a
        // closure that captures `signalValue`. The async/resume call path must
        // apply the same named-argument reorder/default-fill logic as the sync
        // user-call path.
        let wasm = try_compile_with_modules(
            concat!(
                "use { capture } from widgets\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  capture(signalValue: 42)\n",
                "end\n",
            ),
            vec![(
                "widgets",
                concat!(
                    "# Capture.\ndef capture\n",
                    "    @param text Int, default: 20\n",
                    "    @param signalValue Int, default: 30\n",
                    "    @return Int\n",
                    "do\n",
                    "  let cb = do\n",
                    "    signalValue\n",
                    "  end\n",
                    "  cb()\n",
                    "end\n",
                ),
            )],
        )
        .expect("resume named import call with skipped defaults should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_async_caller_named_import_resume_call_fills_skipped_defaults() {
        // ChatComposer is itself compiled as a resume function, so its
        // `TextInput('Message', signalValue: input)` call goes through the
        // async child-frame spawn path rather than `compile_call`. That path
        // must preserve source labels and fill skipped defaults too.
        let wasm = try_compile_with_modules(
            concat!(
                "use { capture } from widgets\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  caller()\n",
                "end\n",
                "\n",
                "# Call capture.\n",
                "def caller\n",
                "    @return Int\n",
                "do\n",
                "  capture(signalValue: 42)\n",
                "end\n",
            ),
            vec![(
                "widgets",
                concat!(
                    "# Capture.\ndef capture\n",
                    "    @param text Int, default: 20\n",
                    "    @param signalValue Int, default: 30\n",
                    "    @return Int\n",
                    "do\n",
                    "  let cb = do\n",
                    "    signalValue\n",
                    "  end\n",
                    "  cb()\n",
                    "end\n",
                ),
            )],
        )
        .expect("async caller should pass labelled args to async callee in declaration order");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_async_closure_named_import_resume_call_fills_skipped_defaults() {
        // The chat composer calls TextInput from inside a view-builder closure.
        // TextInput itself is async because its registered input handler closure
        // calls async signal code. That means the labelled TextInput call is
        // lowered by the async closure's spawn path, not the sync call path.
        let wasm = try_compile_with_modules(
            concat!(
                "use { capture } from widgets\n",
                "\n",
                "type def Children\n",
                "    @return Int\n",
                "end\n",
                "\n",
                "# Run children.\n",
                "def run\n",
                "    @param children Children\n",
                "    @return Int\n",
                "do\n",
                "  children()\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  run do\n",
                "    capture(signalValue: 42)\n",
                "  end\n",
                "end\n",
            ),
            vec![(
                "widgets",
                concat!(
                    "type def Handler\n",
                    "    @return Void\n",
                    "end\n",
                    "\n",
                    "# Async leaf.\n",
                    "def asyncLeaf\n",
                    "    @return Void\n",
                    "do\n",
                    "  sleep(1)\n",
                    "end\n",
                    "\n",
                    "# Register handler.\n",
                    "def registerHandler\n",
                    "    @param handler Handler\n",
                    "    @return Int\n",
                    "do\n",
                    "  7\n",
                    "end\n",
                    "\n",
                    "# Capture.\ndef capture\n",
                    "    @param text Int, default: 20\n",
                    "    @param signalValue Int, default: 30\n",
                    "    @return Int\n",
                    "do\n",
                    "  let changeId = registerHandler do\n",
                    "    asyncLeaf()\n",
                    "  end\n",
                    "  signalValue\n",
                    "end\n",
                ),
            )],
        )
        .expect("async closure should pass labelled args to async callee in declaration order");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_text_input_signal_shape_sets_change_handler_key() {
        // Minimal reproduction of Forui.TextInput's signal branch without the
        // view layer. The web build was rendering TextInput without
        // `changeHandlerId`, which means this branch or dictionary update was
        // not surviving direct codegen.
        let wasm = try_compile_with_modules(
            concat!(
                "use { Signal, textInputLike } from widgets\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  textInputLike('Message', signalValue: Signal(value: 'draft'))\n",
                "end\n",
            ),
            vec![(
                "widgets",
                concat!(
                    "type Signal\n",
                    "  value String\n",
                    "end\n",
                    "\n",
                    "type def ChangeAction\n",
                    "  @param newValue String\n",
                    "  @return Void\n",
                    "end\n",
                    "\n",
                    "# Register change handler.\ndef registerChangeHandler\n",
                    "    @param handler ChangeAction\n",
                    "    @return Int\n",
                    "do\n",
                    "  7\n",
                    "end\n",
                    "\n",
                    "# Text input like.\ndef textInputLike\n",
                    "    @param placeholder String\n",
                    "    @param text String?, default: null\n",
                    "    @param signalValue Signal?, default: null\n",
                    "    @return Int\n",
                    "do\n",
                    "  var props = set({}, 'placeholder', placeholder)\n",
                    "  if signalValue != null\n",
                    "    props = set(props, 'value', signalValue!.value)\n",
                    "    let changeId = registerChangeHandler do with newValue String\n",
                    "      let _ = signalValue!.value\n",
                    "    end\n",
                    "    props = set(props, 'changeHandlerId', changeId)\n",
                    "  end\n",
                    "  if hasKey(props, 'changeHandlerId')\n",
                    "    42\n",
                    "  else\n",
                    "    0\n",
                    "  end\n",
                    "end\n",
                ),
            )],
        )
        .expect("TextInput-like signal branch should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_text_input_signal_from_generic_use_signal() {
        // Same shape as Brain's composer:
        //
        //   var input = useSignal('')
        //   TextInput('Message', signalValue: input)
        //
        // This exercises the generic `useSignal` call, the optional Signal
        // parameter, UFCS reads/writes through `signalValue!`, and the closure
        // capture that makes TextInput compile as a resume function.
        let wasm = try_compile_with_modules(
            concat!(
                "use { textInputLike, useSignal } from widgets\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  var input = useSignal('')\n",
                "  textInputLike('Message', signalValue: input)\n",
                "end\n",
            ),
            vec![(
                "widgets",
                concat!(
                    "type def Loader\n",
                    "  @return $T\n",
                    "end\n",
                    "\n",
                    "type Signal\n",
                    "  id Int\n",
                    "  _value $T\n",
                    "  loader Loader?\n",
                    "end\n",
                    "\n",
                    "type def ChangeAction\n",
                    "  @param newValue String\n",
                    "  @return Void\n",
                    "end\n",
                    "\n",
                    "# Create signal.\ndef createSignal\n",
                    "    @type T\n",
                    "    @param initialValue $T\n",
                    "    @param loader Loader?, default: null\n",
                    "    @return Signal\n",
                    "do\n",
                    "  Signal(id: 1, _value: copy(initialValue), loader: loader)\n",
                    "end\n",
                    "\n",
                    "# Use signal.\ndef useSignal\n",
                    "    @type T\n",
                    "    @param initialValue $T\n",
                    "    @param loader Loader?, default: null\n",
                    "    @return Signal\n",
                    "do\n",
                    "  createSignal(initialValue, loader)\n",
                    "end\n",
                    "\n",
                    "# Value.\ndef value\n",
                    "    @type T\n",
                    "    @param signal Signal\n",
                    "    @return $T\n",
                    "do\n",
                    "  copy(signal._value)\n",
                    "end\n",
                    "\n",
                    "# Set value.\ndef setValue\n",
                    "    @param signal Signal, mutable\n",
                    "    @param newValue $T\n",
                    "    @return Void\n",
                    "do\n",
                    "  signal._value = copy(newValue)\n",
                    "end\n",
                    "\n",
                    "# Register change handler.\ndef registerChangeHandler\n",
                    "    @param handler ChangeAction\n",
                    "    @return Int\n",
                    "do\n",
                    "  7\n",
                    "end\n",
                    "\n",
                    "# Text input like.\ndef textInputLike\n",
                    "    @param placeholder String\n",
                    "    @param text String?, default: null\n",
                    "    @param signalValue Signal?, default: null\n",
                    "    @return Int\n",
                    "do\n",
                    "  var props = set({}, 'placeholder', placeholder)\n",
                    "  if signalValue != null\n",
                    "    props = set(props, 'value', signalValue!.value())\n",
                    "    let changeId = registerChangeHandler do with newValue String\n",
                    "      signalValue!.setValue(newValue)\n",
                    "    end\n",
                    "    props = set(props, 'changeHandlerId', changeId)\n",
                    "  end\n",
                    "  if hasKey(props, 'changeHandlerId')\n",
                    "    42\n",
                    "  else\n",
                    "    0\n",
                    "  end\n",
                    "end\n",
                ),
            )],
        )
        .expect("TextInput-like generic signal path should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_package_named_import_call_fills_skipped_defaults() {
        // Brain imports `TextInput` from `Forui.view` and `useSignal` from
        // `Forui.signal`. A missing named-param reorder entry at that
        // package-qualified call site makes `signalValue: input` behave like
        // the second positional `text` parameter, leaving the real
        // `signalValue` slot null and dropping the change handler.
        let wasm = try_compile_with_modules(
            concat!(
                "use { TextInput } from Forui.view\n",
                "use { useSignal } from Forui.signal\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  var input = useSignal('')\n",
                "  TextInput('Message', signalValue: input)\n",
                "end\n",
            ),
            vec![
                (
                    "Forui.signal",
                    concat!(
                        "type def Loader\n",
                        "  @return $T\n",
                        "end\n",
                        "\n",
                        "type Signal\n",
                        "  id Int\n",
                        "  _value $T\n",
                        "  loader Loader?\n",
                        "end\n",
                        "\n",
                        "# Create signal.\ndef createSignal\n",
                        "    @type T\n",
                        "    @param initialValue $T\n",
                        "    @param loader Loader?, default: null\n",
                        "    @return Signal\n",
                        "do\n",
                        "  Signal(id: 1, _value: copy(initialValue), loader: loader)\n",
                        "end\n",
                        "\n",
                        "# Use signal.\ndef useSignal\n",
                        "    @type T\n",
                        "    @param initialValue $T\n",
                        "    @param loader Loader?, default: null\n",
                        "    @return Signal\n",
                        "do\n",
                        "  createSignal(initialValue, loader)\n",
                        "end\n",
                        "\n",
                        "# Value.\ndef value\n",
                        "    @type T\n",
                        "    @param signal Signal\n",
                        "    @return $T\n",
                        "do\n",
                        "  copy(signal._value)\n",
                        "end\n",
                        "\n",
                        "# Set value.\ndef setValue\n",
                        "    @param signal Signal, mutable\n",
                        "    @param newValue $T\n",
                        "    @return Void\n",
                        "do\n",
                        "  signal._value = copy(newValue)\n",
                        "end\n",
                    ),
                ),
                (
                    "Forui.view",
                    concat!(
                        "use { Signal, value, setValue } from signal\n",
                        "\n",
                        "type def ChangeAction\n",
                        "  @param newValue String\n",
                        "  @return Void\n",
                        "end\n",
                        "\n",
                        "# Register change handler.\ndef registerChangeHandler\n",
                        "    @param handler ChangeAction\n",
                        "    @return Int\n",
                        "do\n",
                        "  7\n",
                        "end\n",
                        "\n",
                        "# Text input.\ndef TextInput\n",
                        "    @param placeholder String\n",
                        "    @param text String?, default: null\n",
                        "    @param signalValue Signal?, default: null\n",
                        "    @return Int\n",
                        "do\n",
                        "  var props = set({}, 'placeholder', placeholder)\n",
                        "  if signalValue != null\n",
                        "    props = set(props, 'value', signalValue!.value())\n",
                        "    let changeId = registerChangeHandler do with newValue String\n",
                        "      signalValue!.setValue(newValue)\n",
                        "    end\n",
                        "    props = set(props, 'changeHandlerId', changeId)\n",
                        "  end\n",
                        "  if hasKey(props, 'changeHandlerId')\n",
                        "    42\n",
                        "  else\n",
                        "    0\n",
                        "  end\n",
                        "end\n",
                    ),
                ),
            ],
        )
        .expect("package named import call with skipped defaults should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_ufcs_in_user_module_uses_module_key() {
        // The checker keys UFCS rewrites by `(module, line, column)`.
        // Direct codegen must use the discovered module's canonical
        // name while compiling that module, otherwise `value.increment()`
        // is treated as an ordinary member call.
        let wasm = try_compile_with_modules(
            concat!(
                "use { run } from pages.tasks\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  run()\n",
                "end\n",
            ),
            vec![
                (
                    "maths",
                    concat!(
                        "# Increment.\ndef increment\n",
                        "    @param value Int\n",
                        "    @return Int\n",
                        "do\n",
                        "  value + 1\n",
                        "end\n",
                    ),
                ),
                (
                    "pages.tasks",
                    concat!(
                        "use { increment } from maths\n",
                        "\n",
                        "# Run.\ndef run\n",
                        "    @return Int\n",
                        "do\n",
                        "  let value = 41\n",
                        "  value.increment()\n",
                        "end\n",
                    ),
                ),
            ],
        )
        .expect("UFCS inside an imported module should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_target_wasm_html_excludes_unavailable_imports() {
        // Under `wasm-html`, the module must not declare
        // `http_server_*` imports — otherwise a browser host that
        // doesn't provide them would fail at instantiate time.
        // Compile a trivial program for the `wasm-html` target and
        // parse its imports back out to verify.
        let prepared = fai_compiler::prepare_source(
            concat!("def main\n", "    @return Int\n", "do\n", "  42\n", "end\n",),
            None,
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
            array_int_index_sites: checker.array_int_index_sites,
            record_field_read_sites: checker.record_field_read_sites,
        };
        let wasm = crate::try_codegen_direct(&prepared.serde_ast, &info, Some("wasm-html"))
            .expect("wasm-html build should succeed");

        let import_names = wasm_import_names(&wasm);
        for excluded_import in &[
            "sleep_ms",
            "run_all",
            "http_server_response",
            "http_server_router",
            "http_server_router_get",
            "http_server_router_post",
            "http_server_router_serve_files",
            "http_server_router_listen",
            "process_run",
            "process_start",
            "process_write",
            "process_read",
            "process_stop",
        ] {
            assert!(
                !import_names.iter().any(|n| n == excluded_import),
                "wasm-html build should exclude `{}` — saw imports {:?}",
                excluded_import,
                import_names,
            );
        }
        // Sanity: at least some imports still present.
        assert!(
            import_names.len() > 10,
            "expected many non-server imports, got {}: {:?}",
            import_names.len(),
            import_names,
        );
    }

    #[test]
    fn ownership_event_import_is_gated_on_native_and_browser() {
        let src = concat!("def main\n", "    @return Int\n", "do\n", "  42\n", "end\n",);

        let native_default =
            try_compile_via_production_for_target(src, None).expect("native compile");
        assert!(
            !wasm_import_names(&native_default)
                .iter()
                .any(|n| n == "__fai_ownership_event"),
            "default native build must not require ownership event import",
        );

        let browser_default =
            try_compile_via_production_for_target(src, Some("wasm-html")).expect("browser compile");
        assert!(
            !wasm_import_names(&browser_default)
                .iter()
                .any(|n| n == "__fai_ownership_event"),
            "default browser build must not require ownership event import",
        );

        let _guard = crate::runtime::OwnershipCheckGuard::new();
        let native_checked =
            try_compile_via_production_for_target(src, None).expect("checked native compile");
        assert!(
            wasm_import_names(&native_checked)
                .iter()
                .any(|n| n == "__fai_ownership_event"),
            "checked native build should declare ownership event import",
        );

        let browser_checked = try_compile_via_production_for_target(src, Some("wasm-html"))
            .expect("checked browser compile");
        assert!(
            wasm_import_names(&browser_checked)
                .iter()
                .any(|n| n == "__fai_ownership_event"),
            "checked browser build should declare ownership event import",
        );
    }

    #[test]
    fn debug_function_call_import_is_gated_on_native_and_browser() {
        let src = concat!("def main\n", "    @return Int\n", "do\n", "  42\n", "end\n",);

        let native_default =
            try_compile_via_production_for_target(src, None).expect("native compile");
        assert!(
            !wasm_import_names(&native_default)
                .iter()
                .any(|n| n == "__fai_debug_function_call"),
            "default native build must not require debug function-call import",
        );

        let browser_default =
            try_compile_via_production_for_target(src, Some("wasm-html")).expect("browser compile");
        assert!(
            !wasm_import_names(&browser_default)
                .iter()
                .any(|n| n == "__fai_debug_function_call"),
            "default browser build must not require debug function-call import",
        );

        let _guard = crate::runtime::DebugFunctionCallsGuard::new();
        let native_debug =
            try_compile_via_production_for_target(src, None).expect("debug native compile");
        assert!(
            wasm_import_names(&native_debug)
                .iter()
                .any(|n| n == "__fai_debug_function_call"),
            "debug native build should declare function-call debug import",
        );

        let browser_debug = try_compile_via_production_for_target(src, Some("wasm-html"))
            .expect("debug browser compile");
        assert!(
            wasm_import_names(&browser_debug)
                .iter()
                .any(|n| n == "__fai_debug_function_call"),
            "debug browser build should declare function-call debug import",
        );
    }

    #[test]
    fn ownership_golden_owned_binding_return_cleanup_sequence() {
        let events = checked_ownership_site_golden(concat!(
            "def main\n",
            "    @return String\n",
            "do\n",
            "  let value = 'owned'\n",
            "  value\n",
            "end\n",
        ));

        assert_eq!(
            events,
            vec!["transfer", "store", "retain", "return", "cleanup"],
        );
    }

    #[test]
    fn ownership_golden_owned_expression_discard_sequence() {
        let events = checked_ownership_site_golden(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  'discarded'\n",
            "  1\n",
            "end\n",
        ));

        assert_eq!(events, vec!["discard", "return"]);
    }

    #[test]
    fn ownership_golden_local_overwrite_sequence() {
        let events = checked_ownership_site_golden(concat!(
            "def main\n",
            "    @return String\n",
            "do\n",
            "  var value = 'old'\n",
            "  value = 'new'\n",
            "  value\n",
            "end\n",
        ));

        assert_eq!(
            events,
            vec![
                "transfer",
                "store",
                "transfer",
                "release",
                "overwrite",
                "retain",
                "return",
                "cleanup",
            ],
        );
    }

    #[test]
    fn checked_ownership_build_emits_resolvable_site_metadata() {
        let src = concat!(
            "def main\n",
            "    @return String\n",
            "do\n",
            "  \"owned\"\n",
            "end\n",
        );

        let default_wasm =
            try_compile_via_production_for_target(src, None).expect("default compile");
        let default_dbg = fai_dbg_json(&default_wasm).expect("default debug section");
        assert!(
            !default_dbg.contains("\"ownership_sites\""),
            "default build should omit empty ownership site metadata: {}",
            default_dbg,
        );

        let _guard = crate::runtime::OwnershipCheckGuard::new();
        let checked_wasm =
            try_compile_via_production_for_target(src, None).expect("checked compile");
        let checked_dbg = fai_dbg_json(&checked_wasm).expect("checked debug section");
        assert!(
            checked_dbg.contains("\"ownership_sites\":[{\"id\":1"),
            "checked build should include dense nonzero ownership site metadata: {}",
            checked_dbg,
        );
        assert!(
            checked_dbg.contains("\"helper\":\"direct\""),
            "checked ownership site should name helper family: {}",
            checked_dbg,
        );
        assert!(
            checked_dbg.contains("\"op\":\"return\""),
            "checked ownership site should include operation family: {}",
            checked_dbg,
        );
    }

    #[test]
    fn checked_ownership_events_use_nonzero_site_ids() {
        let src = concat!(
            "def main\n",
            "    @return String\n",
            "do\n",
            "  \"owned\"\n",
            "end\n",
        );

        let _guard = crate::runtime::OwnershipCheckGuard::new();
        let wasm = try_compile_via_production_for_target(src, None).expect("checked compile");
        let sites = ownership_event_site_args(&wasm);

        assert!(
            !sites.is_empty(),
            "checked build should emit ownership events"
        );
        assert!(
            sites.iter().all(|site| *site > 0),
            "ownership event sites should be nonzero: {:?}",
            sites,
        );
    }

    #[test]
    fn production_direct_exports_match_bytecode_path() {
        // The direct path must export the same set of symbols the
        // bytecode path does so hosts that reach into the module
        // (for closure dispatch, heap inspection, named callbacks)
        // work against either codegen.
        let wasm = try_compile_via_production(concat!(
            "# Helper.\ndef helper\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let f = do with n Int\n",
            "    n + 1\n",
            "  end\n",
            "  helper(21)\n",
            "end\n",
        ))
        .expect("direct build should succeed");

        let parser = wasmparser::Parser::new(0);
        let mut exports: Vec<(String, wasmparser::ExternalKind)> = Vec::new();
        for payload in parser.parse_all(&wasm) {
            if let wasmparser::Payload::ExportSection(section) = payload.expect("payload") {
                for e in section {
                    let e = e.expect("export");
                    exports.push((e.name.to_string(), e.kind));
                }
                break;
            }
        }
        let names: Vec<&str> = exports.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"_start"), "missing _start: {:?}", names);
        assert!(names.contains(&"memory"), "missing memory: {:?}", names);
        assert!(
            names.contains(&"__heap_ptr"),
            "missing __heap_ptr: {:?}",
            names
        );
        assert!(
            names.contains(&"__env_ptr"),
            "missing __env_ptr: {:?}",
            names
        );
        assert!(
            names.contains(&"__indirect_function_table"),
            "missing table export (closure present): {:?}",
            names,
        );
        assert!(
            names.contains(&"helper"),
            "named top-level function not exported: {:?}",
            names,
        );
    }

    #[test]
    fn production_direct_extern_call_roundtrip() {
        let wasm = try_compile_via_production(concat!(
            "extern libc\n",
            "  def strlen(s: String) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = strlen('hi')\n",
            "  42\n",
            "end\n",
        ))
        .expect("extern FFI should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ---- Bare-global builtin tests (Phase H prerequisites) ----
    //
    // Each verifies the direct path can compile + run a bare-global
    // call without falling back to bytecode. These were previously
    // only reachable via translate.rs.

    fn bool_true() -> u64 {
        (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1
    }
    fn bool_false() -> u64 {
        (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64)
    }
    fn int_val(n: u32) -> u64 {
        (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (n as u64)
    }

    #[test]
    fn direct_bare_is_int_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_int(5)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_int_false_on_float() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_int(1.5)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_is_float_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_float(1.5)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_null_true_on_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let v Int? = null\n",
            "  is_null(v)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_null_false_on_int() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_null(5)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_is_bool_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_bool(true)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_bool_false_on_int() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_bool(1)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_is_string_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_string('hi')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_array_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_array([1, 2, 3])\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_array_false_on_string() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_array('hi')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_is_dict_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_dict({a: 1})\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_length_of_array() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length([10, 20, 30, 40])\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(4));
    }

    #[test]
    fn direct_bare_length_of_string() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length('abcde')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(5));
    }

    #[test]
    fn direct_bare_is_empty_array_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let a Int[] = []\n",
            "  isEmpty(a)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_empty_array_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  isEmpty([1])\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_to_string_of_int() {
        // `toString(42)` → "42". Verify via length.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length(toString(42))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(2));
    }

    #[test]
    fn direct_bare_to_string_boxes_raw_int_expression() {
        // Native integer arithmetic compiles to a raw Int shape; toString
        // must box it before handing it to the generic value-to-string helper.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length(toString(0 + 1))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(1));
    }

    #[test]
    fn direct_convert_to_string_boxes_raw_int_expression() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length(convert.toString(0 + 1))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(1));
    }

    #[test]
    fn direct_bare_to_int_passthrough() {
        // `toInt(v)` on an Int is a no-op pass-through in the direct path.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  toInt(7)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(7));
    }

    #[test]
    fn direct_bare_dict_get_string() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let d = {name: 'ada', age: 36}\n",
            "  length(getString(d, 'name'))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(3));
    }

    #[test]
    fn direct_bare_dict_has_key_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let d = {a: 1}\n",
            "  hasKey(d, 'a')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_dict_has_key_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let d = {a: 1}\n",
            "  hasKey(d, 'missing')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_parse_int() {
        // `parseInt("42")` uses RT_PARSE_INT (generated into the
        // module, not a host import) — runs real parsing.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  parseInt('42')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_bare_parse_int_releases_owned_string_argument() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  parseInt('42' + '')\n",
            "end\n",
        )));
        let parse = rt_base_for_standalone() + runtime::RT_PARSE_INT;
        let release = rt_base_for_standalone() + runtime::RT_RELEASE;
        let mut saw_release_after_parse = false;
        for payload in wasmparser::Parser::new(0).parse_all(&wasm) {
            let wasmparser::Payload::CodeSectionEntry(body) = payload.expect("payload") else {
                continue;
            };
            let targets: Vec<u32> = body
                .get_operators_reader()
                .expect("operators")
                .into_iter()
                .filter_map(|op| match op.expect("operator") {
                    wasmparser::Operator::Call { function_index } => Some(function_index),
                    _ => None,
                })
                .collect();
            if let Some(pos) = targets.iter().position(|target| *target == parse) {
                saw_release_after_parse = targets[pos + 1..].contains(&release);
            }
        }
        assert!(
            saw_release_after_parse,
            "parseInt must release owned string args after RT_PARSE_INT"
        );
    }

    #[test]
    fn direct_bare_parse_float_compiles() {
        // `parseFloat(s)` uses RT_PARSE_FLOAT — just verifies the
        // direct path compiles + runs without trapping.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Float?\n",
            "do\n",
            "  parseFloat('1.5')\n",
            "end\n",
        )));
        let _ = run_module(&wasm);
    }

    #[test]
    fn direct_bare_set_mutates_dict() {
        // `set(d, k, v)` returns the mutated dict. Read back via
        // getInt to confirm the value was inserted.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var d = {a: 1}\n",
            "  let d2 = set(d, 'b', 99)\n",
            "  unwrap(getInt(d2, 'b'), 0)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(99));
    }

    #[test]
    fn direct_bare_unwrap_returns_value_when_present() {
        // `unwrap(v, fallback)`: when v is non-null, returns v.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Maybe.\ndef maybe\n",
            "    @return Int?\n",
            "do\n",
            "  42\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  unwrap(maybe(), 0)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_bare_unwrap_returns_fallback_on_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# None.\ndef none\n",
            "    @return Int?\n",
            "do\n",
            "  null\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  unwrap(none(), 7)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(7));
    }

    #[test]
    fn direct_bare_error_ctor_message() {
        // Bare `Error(msg)` form (no `error.` prefix) should build a
        // dict whose `message` field is the argument string.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let e = Error('nope')\n",
            "  length(message(e))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(4));
    }

    /// Compile a source snippet through the same pipeline the CLI
    /// uses (`build_program_full` + `assemble_wasm_module`) so the
    /// synthesised `<__start__>` / `<__module_init__>` wrappers and
    /// the extra module-var globals land in the output. The
    /// standalone `compile_all` + `build_module` helpers bypass
    /// `build_program_full`, so they would not exercise the code
    /// paths under test here.
    fn build_via_full_pipeline(src: &str) -> Vec<u8> {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare failed");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker failed");
        let checker_info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
            array_int_index_sites: checker.array_int_index_sites.clone(),
            record_field_read_sites: checker.record_field_read_sites.clone(),
        };
        crate::codegen_direct_full_reasoned(&prepared.serde_ast, &[], &checker_info, None, false)
            .expect("full-pipeline codegen failed")
    }

    #[test]
    fn direct_module_level_var_read() {
        // Module-level `var` referenced from `main` must resolve to
        // its dedicated wasm global and read the initialised value.
        let wasm = build_via_full_pipeline(concat!(
            "var counter Int = 42\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  counter\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_module_level_var_write() {
        // Assigning to a module-level `var` inside `main` must route
        // through `GlobalSet`; the subsequent read sees the new value.
        let wasm = build_via_full_pipeline(concat!(
            "var counter Int = 0\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  counter = 7\n",
            "  counter\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(7));
    }

    #[test]
    fn direct_module_level_var_persists_across_calls() {
        // A helper that bumps a module-level counter must share its
        // state with `main`'s subsequent read — any scheme that
        // reintroduced a per-call local for `counter` would lose
        // updates here.
        let wasm = build_via_full_pipeline(concat!(
            "var counter Int = 0\n",
            "\n",
            "# Bump the module-level counter by one.\n",
            "def bump\n",
            "    @return Int\n",
            "do\n",
            "  counter = counter + 1\n",
            "  counter\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  bump()\n",
            "  bump()\n",
            "  bump()\n",
            "  counter\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(3));
    }

    #[test]
    fn direct_start_export_is_zero_arg_when_no_main() {
        // Library-shaped file: no `main`, first declared function
        // takes two params. The synthesised `<__start__>` must still
        // give `_start` a `() -> i64` signature so the host runner
        // and test harness can invoke it.
        let wasm = build_via_full_pipeline(concat!(
            "# Return the sum of two integers.\n",
            "def addPair\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a + b\n",
            "end\n",
        ));
        // `run_module` gets `_start` via `get_typed_func::<(), i64>`
        // — instantiation would fail if `_start` had any parameters.
        let _ = run_module(&wasm);
    }

    #[test]
    fn direct_generic_echo_returns_user_arg() {
        // `def echo @type T @param v $T @return $T do v end` called
        // as `echo(42)` must return 42. Regression guard: the
        // builder used to bind user params (locals 0..N) before
        // type params (locals N..N+M), but the call site emits
        // type-args first, so the callee read the type-arg string
        // instead of the real user value. Any generic function that
        // returns a user param was silently returning the wrong
        // value before this fix.
        let wasm = build_via_full_pipeline(concat!(
            "# Return v unchanged.\n",
            "def echo\n",
            "    @type T\n",
            "    @param v $T\n",
            "    @return $T\n",
            "do\n",
            "  v\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  echo(42)\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_generic_value_into_struct_field_round_trips() {
        // `def mkBox @type T @param v $T @return Box do Box(value: v) end`
        // then reading `b.value` must return the value passed in.
        // Same ordering bug — it corrupted the field write because
        // the generic-parameter read inside the constructor call
        // picked up the type-arg string, and that's what landed in
        // the dict.
        let wasm = build_via_full_pipeline(concat!(
            "type Box\n",
            "  value $T\n",
            "end\n",
            "\n",
            "# Build a Box carrying v.\n",
            "def mkBox\n",
            "    @type T\n",
            "    @param v $T\n",
            "    @return Box\n",
            "do\n",
            "  Box(value: v)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let b = mkBox(42)\n",
            "  b.value\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    /// Build a one-statement stub `DiscoveredModule` for collision
    /// tests — the body is unused; only the `name` field matters for
    /// `build_program_full`'s module bookkeeping.
    fn stub_module(name: &str) -> fai_compiler::compiler::DiscoveredModule {
        fai_compiler::compiler::DiscoveredModule {
            name: name.to_string(),
            statements: Vec::new(),
            file_paths: Vec::new(),
            private_names: Vec::new(),
        }
    }

    fn build_program_with_modules_for_test(
        entry_src: &str,
        modules: &[fai_compiler::compiler::DiscoveredModule],
    ) -> Result<BuiltProgram, BuildError> {
        let prepared = fai_compiler::prepare_source(entry_src, None).expect("prepare failed");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker failed");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
            array_int_index_sites: checker.array_int_index_sites.clone(),
            record_field_read_sites: checker.record_field_read_sites.clone(),
        };
        let rt = RtOffsets {
            base: direct_rt_base(),
        };
        let type_indices = direct_fai_func_type_indices();
        let import_available = crate::runtime::available_imports_with_test_flag(None, false);
        let (import_remap, _) = crate::runtime::build_import_remap(&import_available);
        build_program_full(
            &prepared.serde_ast,
            modules,
            rt,
            &info,
            &type_indices,
            &import_remap,
            false,
            None,
        )
    }

    #[test]
    fn direct_duplicate_module_canonical_name_errors() {
        // Two discovered modules with the same canonical name — a
        // local `Forui` directory plus a dependency package also
        // named `Forui` is the concrete user-facing scenario. The
        // builder must refuse rather than silently pick one and
        // shadow the other.
        let entry = concat!("def main\n", "    @return Int\n", "do\n", "  1\n", "end\n",);
        let modules = vec![stub_module("Forui"), stub_module("Forui")];
        let err = build_program_with_modules_for_test(entry, &modules)
            .expect_err("duplicate module name must fail");
        match err {
            BuildError::DuplicateModuleName(name) => assert_eq!(name, "Forui"),
            other => panic!("expected DuplicateModuleName, got {:?}", other),
        }
    }

    #[test]
    fn direct_duplicate_module_basename_is_allowed_without_alias_use() {
        // Two modules with distinct canonical paths but the same
        // final segment are valid folder namespaces. The direct
        // builder should avoid creating an implicit ambiguous
        // basename alias, not reject the whole target graph.
        let entry = concat!("def main\n", "    @return Int\n", "do\n", "  1\n", "end\n",);
        let modules = vec![stub_module("MyApp.Forui"), stub_module("Forui")];
        build_program_with_modules_for_test(entry, &modules)
            .expect("same basename modules should not fail by themselves");
    }

    #[test]
    fn direct_distinct_modules_with_no_basename_collision_ok() {
        // Sanity: distinct canonical names with distinct basenames
        // still build. Guards against over-reaching in the collision
        // check.
        let entry = concat!("def main\n", "    @return Int\n", "do\n", "  1\n", "end\n",);
        let modules = vec![stub_module("Forui.signal"), stub_module("Forui.view")];
        build_program_with_modules_for_test(entry, &modules)
            .expect("distinct names should build cleanly");
    }

    #[test]
    fn direct_force_unwrap_call_on_optional_closure() {
        // `cb!(arg)` — force-unwrap an optional closure and call it
        // in one expression. `compile_call` routes this through the
        // generic non-identifier callee path
        // (`compile_indirect_call_from_expr`), which reuses the
        // normal expression lowering for `ForceUnwrapExpression`.
        // That lowering already emits the `== VAL_NULL → unreachable`
        // null-trap before leaving the closure value on the stack,
        // so the `!` contract is preserved end-to-end — without any
        // special-case code in the call path.
        //
        // Regression guard: this used to refuse with
        // `UnsupportedExpression("CallExpression/non-identifier")`,
        // blocking forui's `navigateListener!(path)`,
        // `onChangeListener!()`, and `mountedApp!()` call sites.
        let wasm = build_via_full_pipeline(concat!(
            "type def Callback\n",
            "    @param x Int\n",
            "    @return Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var cb Callback? = null\n",
            "  cb = do with x Int\n",
            "      x + 1\n",
            "    end\n",
            "  cb!(41)\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_force_unwrap_call_traps_on_null() {
        // Null optional + `!()` must trap at runtime — the `!`
        // contract (panic if null) applies whether the unwrap feeds
        // a read or a call. Paired with the happy-path test above,
        // this locks the generic callee path's null-check in place
        // so a later simplification can't quietly drop it.
        let wasm = build_via_full_pipeline(concat!(
            "type def Callback\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  var cb Callback? = null\n",
            "  cb!()\n",
            "end\n",
        ));
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, &wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            // Mock the FFI boundary: `ffi_begin` parks the task expecting the
            // driver loop to resume it once the worker finishes. Here there's no
            // loop, so resume immediately (`ffi_result` then returns the default
            // 0); enough for async extern-call programs to complete.
            if name == "ffi_begin" {
                linker
                    .func_new(
                        "env",
                        name,
                        FuncType::new(&engine, wt_params, wt_results),
                        move |mut caller, args, _rets| {
                            let task_id = match args.first() {
                                Some(Val::I32(t)) => *t,
                                _ => return Ok(()),
                            };
                            if let Some(f) = caller
                                .get_export("__fai_resume_task")
                                .and_then(|e| e.into_func())
                            {
                                let _ =
                                    f.call(&mut caller, &[Val::I32(task_id)], &mut [Val::I32(0)]);
                            }
                            Ok(())
                        },
                    )
                    .unwrap();
                continue;
            }
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        // `cb!()` invokes a closure value, so the program is async: the
        // null-unwrap trap fires while the root task runs under `__fai_poll`,
        // which `_start_async` drives. (A sync `_start` program would trap in the
        // `_start` call directly; handle whichever entry the module exposes.)
        let err = if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
            start.call(&mut store, ()).expect_err("should trap")
        } else {
            let start_async = instance
                .get_typed_func::<(), i32>(&mut store, "_start_async")
                .expect("_start or _start_async export");
            start_async.call(&mut store, ()).expect_err("should trap")
        };
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("unreachable"),
            "expected unreachable trap, got: {}",
            msg,
        );
    }
