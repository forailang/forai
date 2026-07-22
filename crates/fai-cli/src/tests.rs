    use super::*;

    const SIMPLE_FAI: &str = concat!(
        "def main\n",
        "    @return Void\n",
        "do\n",
        "  print('hello')\n",
        "end\n",
    );

    const INTERFACE_FAI: &str = concat!(
        "# A greeting function.\n",
        "def greet\n",
        "    @param name String\n",
        "    @return String\n",
        "do\n",
        "  'hello'\n",
        "end\n",
        "\n",
        "def main\n",
        "    @return Void\n",
        "do\n",
        "  print('hi')\n",
        "end\n",
    );

    #[test]
    fn test_parse_string_array_inline() {
        assert_eq!(parse_string_array(r#"["web"]"#), vec!["web".to_string()]);
        assert_eq!(
            parse_string_array(r#"["web", "other"]"#),
            vec!["web".to_string(), "other".to_string()]
        );
        // Tolerant of whitespace and a trailing comma.
        assert_eq!(
            parse_string_array(r#"[ "a" , "b", ]"#),
            vec!["a".to_string(), "b".to_string()]
        );
        // Not an array → empty vec.
        assert_eq!(parse_string_array("\"web\""), Vec::<String>::new());
        assert_eq!(parse_string_array("[]"), Vec::<String>::new());
    }

    #[test]
    fn test_format_codegen_error_includes_actionable_async_hint() {
        let err = fai_codegen_wasm::LocatedBuildError {
            err: fai_codegen_wasm::direct::BuildError::UnsupportedExpression("RangeExpression"),
            file: Some("src/data/wiki/main.fai".to_string()),
            line: Some(17),
            col: Some(5),
            module: None,
        };
        let formatted = format_codegen_error(&err);
        assert!(formatted.contains("src/data/wiki/main.fai"));
        assert!(formatted.contains("line 17:5"));
        assert!(formatted.contains("UnsupportedExpression"));
        assert!(
            formatted.contains("Suggestion:"),
            "formatter should tell a human or agent what to try next:\n{}",
            formatted
        );

        let module_err = fai_codegen_wasm::LocatedBuildError {
            err: fai_codegen_wasm::direct::BuildError::UnsupportedExpression("RangeExpression"),
            file: Some("src/data/wiki/embeddings.fai".to_string()),
            line: Some(232),
            col: Some(1),
            module: Some("data.wiki".to_string()),
        };
        let formatted = format_codegen_error(&module_err);
        assert!(formatted.contains("package: data.wiki"));
        assert!(formatted.contains("src/data/wiki/embeddings.fai"));
    }

    /// Build a minimal `ProjectInfo` with a list of (name, deps,
    /// assets) tuples for the planner / asset tests below. The TOML
    /// parser is exercised separately; these tests want a fixture
    /// they can construct cheaply without round-tripping through TOML.
    fn project_with_targets(targets: &[(&str, Vec<&str>, Vec<(&str, &str)>)]) -> ProjectInfo {
        let mut info = ProjectInfo {
            name: "test".into(),
            version: "0.0.0".into(),
            ..Default::default()
        };
        for (name, deps, assets) in targets {
            let mut sub = SubProject::default();
            sub.required_targets = deps.iter().map(|d| d.to_string()).collect();
            sub.assets = assets
                .iter()
                .map(|(f, t)| (f.to_string(), t.to_string()))
                .collect();
            info.sub_projects.insert(name.to_string(), sub);
        }
        info
    }

    #[test]
    fn test_plan_build_order_single_target_no_deps() {
        let info = project_with_targets(&[("server", vec![], vec![])]);
        let order = plan_build_order(&info, Some("server")).unwrap();
        assert_eq!(order, vec!["server".to_string()]);
    }

    #[test]
    fn test_plan_build_order_dep_built_first() {
        let info =
            project_with_targets(&[("web", vec![], vec![]), ("server", vec!["web"], vec![])]);
        let order = plan_build_order(&info, Some("server")).unwrap();
        assert_eq!(order, vec!["web".to_string(), "server".to_string()]);
    }

    #[test]
    fn test_plan_build_order_transitive_chain() {
        // a → b → c → d (a depends on b which depends on c which …)
        // Building `a` should produce d, c, b, a in that order.
        let info = project_with_targets(&[
            ("a", vec!["b"], vec![]),
            ("b", vec!["c"], vec![]),
            ("c", vec!["d"], vec![]),
            ("d", vec![], vec![]),
        ]);
        let order = plan_build_order(&info, Some("a")).unwrap();
        assert_eq!(
            order,
            vec![
                "d".to_string(),
                "c".to_string(),
                "b".to_string(),
                "a".to_string()
            ]
        );
    }

    #[test]
    fn test_plan_build_order_diamond() {
        // a → b, a → c, b → d, c → d. d must come first; a must come
        // last; b and c can be in either order between them. Each
        // target should appear exactly once (no double-build of d).
        let info = project_with_targets(&[
            ("a", vec!["b", "c"], vec![]),
            ("b", vec!["d"], vec![]),
            ("c", vec!["d"], vec![]),
            ("d", vec![], vec![]),
        ]);
        let order = plan_build_order(&info, Some("a")).unwrap();
        assert_eq!(order.len(), 4, "each target builds exactly once");
        let pos = |t: &str| order.iter().position(|n| n == t).unwrap();
        assert!(pos("d") < pos("b"));
        assert!(pos("d") < pos("c"));
        assert!(pos("b") < pos("a"));
        assert!(pos("c") < pos("a"));
    }

    #[test]
    fn test_plan_build_order_detects_cycle() {
        let info = project_with_targets(&[("a", vec!["b"], vec![]), ("b", vec!["a"], vec![])]);
        let err = plan_build_order(&info, Some("a")).unwrap_err();
        assert!(err.contains("cycle"), "expected cycle error, got: {}", err);
        assert!(err.contains("a") && err.contains("b"));
    }

    #[test]
    fn test_plan_build_order_detects_self_cycle() {
        let info = project_with_targets(&[("a", vec!["a"], vec![])]);
        let err = plan_build_order(&info, Some("a")).unwrap_err();
        assert!(err.contains("cycle"), "expected cycle error, got: {}", err);
    }

    #[test]
    fn test_plan_build_order_unknown_dep_skipped() {
        let info = project_with_targets(&[("server", vec!["nonexistent"], vec![])]);
        // Unknown deps warn but don't fail planning. `server` still
        // builds — the warning gives the user a chance to fix the
        // typo without breaking everyone else's build.
        let order = plan_build_order(&info, Some("server")).unwrap();
        assert_eq!(order, vec!["server".to_string()]);
    }

    #[test]
    fn test_plan_build_order_build_all_alphabetic_roots() {
        // No `requested` → walk every sub-project. Roots are sorted
        // alphabetically for stable ordering across runs. With no
        // deps, the output is just the sorted target list.
        let info = project_with_targets(&[
            ("zeta", vec![], vec![]),
            ("alpha", vec![], vec![]),
            ("middle", vec![], vec![]),
        ]);
        let order = plan_build_order(&info, None).unwrap();
        assert_eq!(
            order,
            vec![
                "alpha".to_string(),
                "middle".to_string(),
                "zeta".to_string()
            ]
        );
    }

    #[test]
    fn test_plan_build_order_build_all_respects_deps() {
        // Build-all walks every target alphabetically as a root, but
        // each root's deps still come before it. Net effect: deps
        // appear before dependents regardless of alphabetical name.
        let info =
            project_with_targets(&[("server", vec!["web"], vec![]), ("web", vec![], vec![])]);
        let order = plan_build_order(&info, None).unwrap();
        let pos_web = order.iter().position(|n| n == "web").unwrap();
        let pos_server = order.iter().position(|n| n == "server").unwrap();
        assert!(pos_web < pos_server, "web must build before server");
    }

    #[test]
    fn test_copy_dir_merge_copies_file_tree() {
        let tmp = temp_dir("copy_dir_merge_tree");
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.txt"), "hello").unwrap();
        std::fs::write(src.join("nested").join("b.txt"), "world").unwrap();

        copy_dir_merge(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            std::fs::read_to_string(dst.join("nested").join("b.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn test_copy_dir_merge_layers_two_sources() {
        // Two sequential copies into the same destination: the second
        // overwrites overlapping files but preserves files unique to
        // the first. This is the exact pattern used by the assets
        // map to layer a generated bundle and a project's public/.
        let tmp = temp_dir("copy_dir_merge_layers");
        let src_a = tmp.join("a");
        let src_b = tmp.join("b");
        let dst = tmp.join("dst");
        std::fs::create_dir_all(&src_a).unwrap();
        std::fs::create_dir_all(&src_b).unwrap();
        std::fs::write(src_a.join("only_a.txt"), "from-a").unwrap();
        std::fs::write(src_a.join("shared.txt"), "a-version").unwrap();
        std::fs::write(src_b.join("only_b.txt"), "from-b").unwrap();
        std::fs::write(src_b.join("shared.txt"), "b-version").unwrap();

        copy_dir_merge(&src_a, &dst).unwrap();
        copy_dir_merge(&src_b, &dst).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("only_a.txt")).unwrap(),
            "from-a"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("only_b.txt")).unwrap(),
            "from-b"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("shared.txt")).unwrap(),
            "b-version",
            "later copy wins on overlap"
        );
    }

    #[test]
    fn test_copy_dir_merge_missing_source_is_noop() {
        let tmp = temp_dir("copy_dir_merge_missing");
        let dst = tmp.join("dst");
        let result = copy_dir_merge(&tmp.join("does_not_exist"), &dst);
        assert!(result.is_ok());
        assert!(!dst.exists(), "destination not created when source missing");
    }

    #[test]
    fn test_copy_target_assets_resolves_target_ref_and_project_path() {
        // Set up a tiny project root with a generated `build/web/`
        // and an authored `public/`, mimicking forailang.com. After
        // copy_target_assets runs, both directories should be merged
        // into `build/server/public/`.
        let root = temp_dir("copy_target_assets");
        std::fs::create_dir_all(root.join("build/web")).unwrap();
        std::fs::write(root.join("build/web/web.wasm"), "wasm-bytes").unwrap();
        std::fs::write(root.join("build/web/forui.css"), "css-bytes").unwrap();
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("public/favicon.ico"), "icon-bytes").unwrap();
        // The server's build_dir must exist (build_one_subproject
        // would have created it via cmd_build); fake that here.
        std::fs::create_dir_all(root.join("build/server")).unwrap();

        let mut web = SubProject::default();
        web.build_dir = Some("build/web".to_string());
        let mut server = SubProject::default();
        server.build_dir = Some("build/server".to_string());
        server.assets = vec![
            ("$web".to_string(), "public".to_string()),
            ("public".to_string(), "public".to_string()),
        ];

        let mut info = ProjectInfo::default();
        info.sub_projects.insert("web".to_string(), web);
        info.sub_projects
            .insert("server".to_string(), server.clone());

        copy_target_assets("server", &server, &info, &root);

        let merged = root.join("build/server/public");
        assert!(merged.join("web.wasm").exists(), "$web/web.wasm copied");
        assert!(merged.join("forui.css").exists(), "$web/forui.css copied");
        assert!(
            merged.join("favicon.ico").exists(),
            "project public/favicon.ico copied"
        );
    }

    #[test]
    fn test_copy_target_assets_empty_to_copies_into_build_dir_root() {
        let root = temp_dir("copy_target_assets_root");
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("public/robots.txt"), "user-agent: *").unwrap();
        std::fs::create_dir_all(root.join("build/server")).unwrap();

        let mut server = SubProject::default();
        server.build_dir = Some("build/server".to_string());
        server.assets = vec![("public".to_string(), "".to_string())];

        let mut info = ProjectInfo::default();
        info.sub_projects
            .insert("server".to_string(), server.clone());

        copy_target_assets("server", &server, &info, &root);

        // Empty `to` → the public/ contents land directly inside
        // build/server/, not nested under build/server/public/.
        assert!(root.join("build/server/robots.txt").exists());
        assert!(!root.join("build/server/public/robots.txt").exists());
    }

    #[test]
    fn test_copy_target_assets_missing_source_does_not_panic() {
        let root = temp_dir("copy_target_assets_missing");
        std::fs::create_dir_all(root.join("build/server")).unwrap();
        let mut server = SubProject::default();
        server.build_dir = Some("build/server".to_string());
        server.assets = vec![("public".to_string(), "public".to_string())];
        let mut info = ProjectInfo::default();
        info.sub_projects
            .insert("server".to_string(), server.clone());

        // No `public/` directory exists. We expect a stderr warning,
        // not a panic — a missing optional asset shouldn't take down
        // the build.
        copy_target_assets("server", &server, &info, &root);
    }

    #[test]
    fn test_parser_planner_assets_e2e() {
        // End-to-end: parse a real fai.toml, plan the build order
        // from it, and run copy_target_assets in dep order. Verifies
        // the three new pieces (parser additions, planner, asset
        // copier) compose correctly when fed by the same `ProjectInfo`
        // they share at runtime — the integration `step_build` does
        // for real but without invoking the wasm compiler.
        let root = temp_dir("e2e_pipeline");
        let toml = concat!(
            "[project]\n",
            "name = \"e2eapp\"\n",
            "\n",
            "[project.web]\n",
            "target = \"wasm-html\"\n",
            "build_dir = \"build/web\"\n",
            "\n",
            "[project.server]\n",
            "target = \"native\"\n",
            "build_dir = \"build/server\"\n",
            "required_targets = [\"web\"]\n",
            "\n",
            "[project.server.assets]\n",
            "\"$web\" = \"public\"\n",
            "\"public\" = \"public\"\n",
        );
        std::fs::write(root.join("fai.toml"), toml).unwrap();
        // Pretend the web build has already deposited its artifacts.
        // build_one_subproject would do this via cmd_build; the
        // planner / asset-copy stages don't care how the bytes got
        // there, only that they exist when the dependent target
        // tries to copy them.
        std::fs::create_dir_all(root.join("build/web")).unwrap();
        std::fs::write(root.join("build/web/web.wasm"), "wasm").unwrap();
        std::fs::write(root.join("build/web/fai-runtime.js"), "js").unwrap();
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("public/favicon.ico"), "icon").unwrap();
        std::fs::create_dir_all(root.join("build/server")).unwrap();

        let info = parse_project_info(toml);
        // Planner: dep order is web → server.
        let order = plan_build_order(&info, Some("server")).unwrap();
        assert_eq!(order, vec!["web".to_string(), "server".to_string()]);

        // Run asset copy in planned order. Web has no assets so this
        // is a no-op; server merges $web + public into build/server/public/.
        for name in &order {
            let sub = info.sub_projects.get(name).unwrap();
            copy_target_assets(name, sub, &info, &root);
        }

        let merged = root.join("build/server/public");
        assert!(merged.join("web.wasm").exists());
        assert!(merged.join("fai-runtime.js").exists());
        assert!(merged.join("favicon.ico").exists());
    }

    #[test]
    fn test_resolve_target_wasm_artifact_returns_path_when_present() {
        // Cargo runs tests in parallel and `current_dir` is
        // process-global, so any test that mutates cwd must hold the
        // shared lock for the duration of its cwd window.
        let _guard = cwd_test_lock();
        let root = temp_dir("resolve_artifact_present");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"app\"\n\n[project.server]\nbuild_dir = \"build/server\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("build/server")).unwrap();
        std::fs::write(root.join("build/server/server.wasm"), "wasm").unwrap();

        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        let resolved = resolve_target_wasm_artifact(Some("server"));
        std::env::set_current_dir(&original).unwrap();

        let path = resolved.expect("artifact resolves when present");
        assert!(path.ends_with("build/server/server.wasm"), "got {}", path);
    }

    #[test]
    fn test_resolve_target_wasm_artifact_returns_none_when_not_built() {
        let _guard = cwd_test_lock();
        let root = temp_dir("resolve_artifact_missing");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"app\"\n\n[project.server]\nbuild_dir = \"build/server\"\n",
        )
        .unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        let resolved = resolve_target_wasm_artifact(Some("server"));
        std::env::set_current_dir(&original).unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn test_parse_required_targets_and_assets() {
        let toml = concat!(
            "[project]\n",
            "name = \"app\"\n",
            "\n",
            "[project.web]\n",
            "target = \"wasm-html\"\n",
            "build_dir = \"build/web\"\n",
            "\n",
            "[project.server]\n",
            "target = \"native\"\n",
            "build_dir = \"build/server\"\n",
            "required_targets = [\"web\"]\n",
            "\n",
            "[project.server.assets]\n",
            "\"$web\" = \"public\"\n",
            "\"public\" = \"public\"\n",
        );
        let info = parse_project_info(toml);
        let server = info
            .sub_projects
            .get("server")
            .expect("server target parsed");
        assert_eq!(server.required_targets, vec!["web".to_string()]);
        assert_eq!(
            server.assets,
            vec![
                ("$web".to_string(), "public".to_string()),
                ("public".to_string(), "public".to_string()),
            ]
        );
        // The web target has no required_targets / assets — just here
        // to confirm the parser keeps them empty rather than borrowing
        // them from a sibling section.
        let web = info.sub_projects.get("web").expect("web target parsed");
        assert!(web.required_targets.is_empty());
        assert!(web.assets.is_empty());
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fai_cli_test_{}", tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    // ── Plan 116 phase 2: watchdog + post-mortem dump ──

    /// Compile a source string written to `<temp>/main.fai`.
    fn compile_snippet(tag: &str, src: &str) -> Vec<u8> {
        let dir = temp_dir(tag);
        let path = dir.join("main.fai");
        std::fs::write(&path, src).unwrap();
        compile_fai_to_wasm(src, path.to_str().unwrap(), false, Vec::new(), None, None)
    }

    #[test]
    fn watchdog_dump_names_the_waiting_tasks() {
        // `main` awaits `never`, which parks on a 60s timer in a loop —
        // the run can't complete. The watchdog must kill it and the
        // post-mortem dump must name the parked task and its waiter.
        let wasm = compile_snippet(
            "watchdog_dump",
            concat!(
                "# Parks forever: sleeps in a loop and never returns.\n",
                "def never\n",
                "    @return Int\n",
                "do\n",
                "    while true\n",
                "        sleep(60000)\n",
                "    end\n",
                "    return 1\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    let x = never()\n",
                "    print(x)\n",
                "end\n",
            ),
        );
        let err = wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                watchdog_secs: Some(1),
                ..Default::default()
            },
        )
        .expect_err("watchdog should kill the parked program");
        // Two watchdog paths can fire first: the elapsed check between
        // polls ("no completion after Ns") or the epoch interrupt
        // landing mid-poll ("still running after Ns — interrupted").
        // Either way the dump must name the parked task and its waiter.
        assert!(err.contains("watchdog"), "{err}");
        assert!(err.contains("never#resume"), "{err}");
        assert!(err.contains("WAITING"), "{err}");
        assert!(err.contains("t1"), "{err}");
    }

    #[test]
    fn watchdog_interrupts_a_sync_infinite_loop() {
        // A sync `while true` never reaches a host call, so only epoch
        // interruption can break it. The report carries the watchdog
        // reason plus the post-mortem heap stats.
        let wasm = compile_snippet(
            "watchdog_spin",
            concat!(
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var i = 0\n",
                "    while true\n",
                "        i = i + 1\n",
                "        if i > 1000000\n",
                "            i = 0\n",
                "        end\n",
                "    end\n",
                "end\n",
            ),
        );
        let err = wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                watchdog_secs: Some(1),
                ..Default::default()
            },
        )
        .expect_err("watchdog should interrupt the spinning program");
        assert!(
            err.contains("watchdog: still running after 1s — interrupted"),
            "{err}",
        );
        assert!(err.contains("post-mortem:"), "{err}");
    }

    #[test]
    fn trap_in_async_run_includes_post_mortem_task_table() {
        // A trap inside an async program appends the task-table dump to
        // the decorated backtrace — no watchdog involved.
        let wasm = compile_snippet(
            "trap_post_mortem",
            concat!(
                "# Sleeps then unwraps null.\n",
                "def crashLater\n",
                "    @return Int\n",
                "do\n",
                "    var x Int? = null\n",
                "    return x!\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    sleep(5)\n",
                "    let v = crashLater()\n",
                "    print(v)\n",
                "end\n",
            ),
        );
        let err = wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions::default(),
        )
        .expect_err("force-unwrap of null should trap");
        assert!(err.contains("force-unwrap"), "{err}");
        // The frame carries the (temp-dir-qualified) file and line.
        assert!(err.contains("crashLater ("), "{err}");
        assert!(err.contains("main.fai:"), "{err}");
        assert!(err.contains("post-mortem:"), "{err}");
        assert!(err.contains("main#resume"), "{err}");
    }

    // ── Plan 116 phase 5: `--check-leaks` heap allocation ledger ──

    /// Run a `--check-leaks` build and return the captured stderr
    /// (where the ledger report lands). Codegen instrumentation comes
    /// from the thread-local guard; compile and run share this thread.
    fn run_with_check_leaks(tag: &str, src: &str) -> String {
        let _cg = fai_codegen_wasm::CheckLeaksGuard::new();
        let wasm = compile_snippet(tag, src);
        let guard = wasm_runner::output::CaptureGuard::new();
        let result = wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                check_leaks: Some(wasm_runner::CheckLeaksOptions::default()),
                ..Default::default()
            },
        );
        let stderr = guard.stderr();
        drop(guard);
        result.expect("check-leaks program should run to completion");
        stderr
    }

    #[test]
    fn check_leaks_report_names_the_leaking_function() {
        // Ten same-size strings escape into a program-lifetime global —
        // the live set at exit. Tier 1: the report shows the group with
        // its count; Tier 2a: the allocation site names `makeLeak` (via
        // the backtrace captured at each rt_alloc). The self-check
        // against `__live_objects` must agree.
        let stderr = run_with_check_leaks(
            "check_leaks_named",
            concat!(
                "use std.array\n",
                "\n",
                "var cache String[] = []\n",
                "\n",
                "# Allocates strings that stay referenced by the global cache.\n",
                "def makeLeak\n",
                "    @return Void\n",
                "do\n",
                "    var i = 0\n",
                "    while i < 10\n",
                "        cache = array.append(cache, 'leak-string-payload-' + toString(i))\n",
                "        i = i + 1\n",
                "    end\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    makeLeak()\n",
                "    print(length(cache))\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("[check-leaks] live heap:"), "{stderr}");
        // Tier 1: ten leaked strings of one size, grouped.
        assert!(stderr.contains("\n     10 "), "{stderr}");
        assert!(stderr.contains("String"), "{stderr}");
        // Tier 2a: the allocation site names the leaking function.
        assert!(stderr.contains("makeLeak"), "{stderr}");
        // Self-check: ledger and __live_objects agree.
        assert!(stderr.contains("consistent"), "{stderr}");
    }

    #[test]
    fn check_leaks_clean_program_reports_empty_live_set() {
        // A loop that builds and drops temporaries must come back to an
        // empty live set — the ledger version of the reclaim fixtures.
        let stderr = run_with_check_leaks(
            "check_leaks_clean",
            concat!(
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var i = 0\n",
                "    while i < 200\n",
                "        let label = 'item-' + toString(i)\n",
                "        i = i + 1\n",
                "    end\n",
                "    print('done')\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("live heap: 0 objects, 0 bytes"), "{stderr}",);
        assert!(stderr.contains("consistent"), "{stderr}");
    }

    #[test]
    fn check_leaks_async_loop_bindings_are_clean() {
        // Regression for the async-frame loop leak (the brain SSR
        // ~15KB/request): a suspending loop's `let`, its awaited call
        // result, and a `html = html + part` accumulator must all be
        // reclaimed per iteration — the live set at exit contains no
        // per-iteration strings. (The one allowed survivor is the
        // scheduler's one-time startup allocation.)
        let stderr = run_with_check_leaks(
            "check_leaks_async_loop",
            concat!(
                "# Returns a fresh heap string after suspending.\n",
                "def apiece\n",
                "    @param i Int\n",
                "    @return String\n",
                "do\n",
                "    sleep(0)\n",
                "    'piece-' + toString(i)\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var html = ''\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        let part = apiece(i)\n",
                "        html = html + part\n",
                "        i = i + 1\n",
                "    end\n",
                "    print(length(html))\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("[check-leaks] live heap:"), "{stderr}");
        assert!(stderr.contains("consistent"), "{stderr}");
        // No per-iteration leak groups: neither the awaited results nor
        // the accumulator intermediates survive to the exit report.
        assert!(!stderr.contains("apiece"), "{stderr}");
        assert!(!stderr.contains("\n     29 "), "{stderr}");
        assert!(!stderr.contains("\n     30 "), "{stderr}");
    }

    #[test]
    fn check_leaks_module_peer_call_results_are_clean() {
        // Regression: RC ownership classification must resolve module-peer
        // calls (`piece(i)` inside module `rend` → `rend.piece`) exactly the
        // way `compile_call` resolves them. Misclassified as borrowed, every
        // peer-call result is over-retained on bind / skipped by operand
        // mop-up and leaks once per call — the sync half of the brain SSR
        // per-request leak (plan 116).
        let dir = temp_dir("check_leaks_module_peer");
        // Module discovery roots at fai.toml's source_root.
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"modpeer\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src/rend")).unwrap();
        std::fs::write(
            dir.join("src/rend/rend.fai"),
            concat!(
                "# Returns a fresh concat string.\n",
                "def piece\n",
                "    @param i Int\n",
                "    @return String\n",
                "do\n",
                "    'piece-' + toString(i)\n",
                "end\n",
                "\n",
                "# Wraps a peer-call result (one more sync call level).\n",
                "def wrap\n",
                "    @param i Int\n",
                "    @return String\n",
                "do\n",
                "    let inner = piece(i)\n",
                "    '<' + inner + '>'\n",
                "end\n",
                "\n",
                "# Accumulates peer-call results in a loop.\n",
                "def buildAll\n",
                "    @return Int\n",
                "do\n",
                "    var html = ''\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        html = html + wrap(i)\n",
                "        i = i + 1\n",
                "    end\n",
                "    length(html)\n",
                "end\n",
            ),
        )
        .unwrap();
        let main_src = concat!(
            "use { buildAll } from rend\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "    print(buildAll())\n",
            "end\n",
        );
        let path = dir.join("src/main.fai");
        std::fs::write(&path, main_src).unwrap();
        let _cg = fai_codegen_wasm::CheckLeaksGuard::new();
        let wasm = compile_fai_to_wasm(
            main_src,
            path.to_str().unwrap(),
            false,
            Vec::new(),
            None,
            None,
        );
        let guard = wasm_runner::output::CaptureGuard::new();
        wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                check_leaks: Some(wasm_runner::CheckLeaksOptions::default()),
                ..Default::default()
            },
        )
        .expect("module program should run");
        let stderr = guard.stderr();
        drop(guard);
        assert!(stderr.contains("consistent"), "{stderr}");
        // No leak group may name the module's functions — every peer-call
        // result (piece's string, wrap's string) is reclaimed.
        assert!(!stderr.contains("rend."), "{stderr}");
    }

    #[test]
    fn check_leaks_std_host_call_results_are_clean() {
        // Regression (plan 116 host-leak pass): std host calls returning
        // fresh object graphs (json.parse/stringify, file.read, env.get)
        // were classified borrowed — over-retained on bind, one leaked
        // graph per call — and file.read leaked its 64 KiB scratch buffer
        // plus an owned literal path temp per call. `file.read` now lowers
        // through the async host-op path, so the known scheduler root may
        // remain live until runtime teardown lands; per-call host results
        // and argument temporaries must not.
        std::fs::write("/tmp/fai_check_leaks_std.txt", "file-content-here").unwrap();
        let stderr = run_with_check_leaks(
            "check_leaks_std_host",
            concat!(
                "use std.env\n",
                "\n",
                "use std.file\n",
                "\n",
                "use std.json\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var i = 0\n",
                "    while i < 20\n",
                "        let v = json.parse('{\"k\": [1, 2, 3], \"s\": \"hello\"}')\n",
                "        let s = json.stringify(v)\n",
                "        let f = file.read('/tmp/fai_check_leaks_std.txt')\n",
                "        let e = env.get('HOME')\n",
                "        i = i + 1\n",
                "    end\n",
                "    print('done')\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("consistent"), "{stderr}");
        let empty = stderr.contains("live heap: 0 objects, 0 bytes");
        let scheduler_root_only =
            stderr.contains("live heap: 1 objects") && stderr.contains("sched_start_async");
        assert!(
            empty || scheduler_root_only,
            "expected no std-host-call leaks beyond the known async scheduler root:\n{stderr}",
        );
        assert!(
            !stderr.contains("main#resume"),
            "async host-op results or argument temporaries leaked per iteration:\n{stderr}",
        );
    }

    #[test]
    fn check_leaks_event_dispatch_is_clean() {
        // Regression (plan 116 host-leak pass): every event dispatch leaked
        // its host-built Event{name,data} dict — `build_event` now retains
        // the data and `dispatch_event` releases the event after the
        // subscribers run. Only the one-time subscription survives.
        let stderr = run_with_check_leaks(
            "check_leaks_events",
            concat!(
                "use std.events\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    let _sub = events.on('tick') do with e Event\n",
                "        let n = e.name\n",
                "    end\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        events.emit('tick', 'payload-' + toString(i))\n",
                "        i = i + 1\n",
                "    end\n",
                "    print('done')\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("consistent"), "{stderr}");
        // No per-dispatch group: 30 leaked events would show as a
        // count-30 line.
        assert!(!stderr.contains("\n     30 "), "{stderr}");
    }

    #[test]
    fn check_leaks_from_dict_binding_is_clean() {
        // Regression (plan 116 host-leak pass): `let x T = from_dict(d)`
        // bound its fresh record without note_droppable — one leaked
        // record per call (brain's beforeRequest listener built one per
        // request, pinning request sub-dicts with it).
        let stderr = run_with_check_leaks(
            "check_leaks_from_dict",
            concat!(
                "type Point\n",
                "    x Int\n",
                "    y Int\n",
                "    label String\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    let src = { x: 1 y: 2 label: 'origin' }\n",
                "    var total = 0\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        let p Point = from_dict(src)\n",
                "        total = total + p.x\n",
                "        i = i + 1\n",
                "    end\n",
                "    print(total)\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("consistent"), "{stderr}");
        assert!(!stderr.contains("\n     30 "), "{stderr}");
        assert!(!stderr.contains("\n     29 "), "{stderr}");
    }

    #[test]
    fn check_leaks_cells_and_async_args_are_clean() {
        // Regression (plan 114 cell unification): captured-mutated vars
        // (cells), their value chains, the closures that capture them,
        // and owned arguments passed to async calls must all reclaim —
        // including a closure that ESCAPES its task and is called after
        // the task completed (the cell outlives the reclaimed frame).
        let stderr = run_with_check_leaks(
            "check_leaks_cells",
            concat!(
                "type def Thunk\n",
                "    @return Void\n",
                "end\n",
                "\n",
                "# Async fn taking a heap arg (param slot owns +1).\n",
                "def measure\n",
                "    @param s String\n",
                "    @return Int\n",
                "do\n",
                "    sleep(0)\n",
                "    length(s)\n",
                "end\n",
                "\n",
                "# Mutates captured cells across suspensions.\n",
                "def runOnce\n",
                "    @param i Int\n",
                "    @return Int\n",
                "do\n",
                "    var acc = ''\n",
                "    let bump = do\n",
                "        acc = acc + 'x'\n",
                "    end\n",
                "    bump()\n",
                "    sleep(0)\n",
                "    bump()\n",
                "    length(acc) + measure('fresh-' + toString(i))\n",
                "end\n",
                "\n",
                "# Returns a closure over a cell; called after the task completes.\n",
                "def makeEscaped\n",
                "    @return Thunk\n",
                "do\n",
                "    var stash = 'payload'\n",
                "    sleep(0)\n",
                "    let esc = do\n",
                "        stash = stash + '!'\n",
                "    end\n",
                "    esc\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var total = 0\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        total = total + runOnce(i)\n",
                "        let escaped = makeEscaped()\n",
                "        escaped()\n",
                "        escaped()\n",
                "        i = i + 1\n",
                "    end\n",
                "    print(total)\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("consistent"), "{stderr}");
        // No per-iteration groups: cells, closures, args all reclaimed.
        assert!(!stderr.contains("\n     30 "), "{stderr}");
        assert!(!stderr.contains("\n     29 "), "{stderr}");
        assert!(!stderr.contains("\n     60 "), "{stderr}");
        assert!(!stderr.contains("Cell"), "{stderr}");
    }

    #[test]
    fn check_leaks_fn_refs_and_tostring_owned_args_are_clean() {
        // Regression (plan 114 tail — brain's last 2 objects/request):
        // (a) a function REFERENCE used as a value compiles to a fresh
        // closure wrapper per use and must transfer ownership (it was
        // classified borrowed and the wrapper leaked once per use);
        // (b) `toString(<owned call result>)` must release its arg temp
        // (the alias-retain made the result +1 but never consumed the
        // owned arg, leaking one copy per call — `toString(s.value())`).
        let stderr = run_with_check_leaks(
            "check_leaks_fnref_tostring",
            concat!(
                "type def Producer\n",
                "    @return Int\n",
                "end\n",
                "\n",
                "# Returns a constant.\n",
                "def piece\n",
                "    @return Int\n",
                "do\n",
                "    7\n",
                "end\n",
                "\n",
                "# Async fn calling a closure-typed param.\n",
                "def callIt\n",
                "    @param f Producer\n",
                "    @return Int\n",
                "do\n",
                "    sleep(0)\n",
                "    f()\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    let base = 'value-string'\n",
                "    var total = 0\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        total = total + callIt(piece)\n",
                "        let s = toString(copy(base))\n",
                "        i = i + 1\n",
                "    end\n",
                "    print(total)\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("consistent"), "{stderr}");
        assert!(!stderr.contains("\n     30 "), "{stderr}");
        assert!(!stderr.contains("\n     29 "), "{stderr}");
        assert!(!stderr.contains("Closure"), "{stderr}");
    }

    #[test]
    fn check_leaks_accounts_for_host_side_allocations() {
        // Host-built objects (json.parse builds the value graph via the
        // host `reserve`, not `rt_alloc`) are recorded with host origin:
        // the self-check offsets them, and the report attributes them.
        let stderr = run_with_check_leaks(
            "check_leaks_host",
            concat!(
                "use std.json\n",
                "\n",
                "var keep = json.parse('{\"name\": \"hello\", \"xs\": [1, 2]}')\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    print('ok')\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("[check-leaks] live heap:"), "{stderr}");
        assert!(stderr.contains("host-side"), "{stderr}");
        assert!(stderr.contains("consistent"), "{stderr}");
        assert!(stderr.contains("host import"), "{stderr}");
    }

    #[test]
    fn check_leaks_on_uninstrumented_module_reports_hint() {
        // A module compiled WITHOUT the flag emits no events; running it
        // with --check-leaks must say so instead of claiming "no leaks".
        let wasm = compile_snippet(
            "check_leaks_uninstrumented",
            "def main\n    @return Void\ndo\n    print('hi')\nend\n",
        );
        let guard = wasm_runner::output::CaptureGuard::new();
        wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                check_leaks: Some(wasm_runner::CheckLeaksOptions::default()),
                ..Default::default()
            },
        )
        .expect("program should run");
        let stderr = guard.stderr();
        drop(guard);
        assert!(stderr.contains("not built with --check-leaks"), "{stderr}",);
    }

    /// Shared mutex for tests that call `set_current_dir` — CWD is
    /// process-global and cargo runs tests in parallel by default.
    /// Acquire this before any `set_current_dir` in a test.
    fn cwd_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn write_fai(tag: &str, src: &str) -> String {
        let dir = temp_dir(tag);
        let path = dir.join("prog.fai");
        std::fs::write(&path, src).unwrap();
        path.to_string_lossy().into_owned()
    }

    // ── try_check_single_file ────────────────────────────────────────

    /// Create a minimal external package with a custom type in a temp directory.
    /// Returns (package_root, package_src_root) as absolute path strings.
    fn make_pkg_with_widget_type(tag: &str) -> std::path::PathBuf {
        let pkg = temp_dir(&format!("{}_pkg", tag));
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::write(
            pkg.join("fai.toml"),
            "[project]\nname = \"WidgetPkg\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\n",
        ).unwrap();
        // Defines a Widget type and a makeWidget constructor.
        std::fs::write(
            pkg.join("src").join("widget.fai"),
            "type Widget\n  label String\nend\n\n# Make a widget.\ndef makeWidget\n    @param label String\n    @return Widget\ndo\n  Widget(label: label)\nend\n",
        ).unwrap();
        pkg
    }

    #[test]
    fn test_try_check_single_file_resolves_external_package_type() {
        // Regression test: when a .fai file has sibling files AND imports types
        // from an external package, try_check_single_file must succeed.
        //
        // The old code called prepare_module_directory which did NOT load fai.toml
        // dependencies, so types like ViewNode were "Unknown type" at check time.
        // The fix uses prepare_source which resolves all deps via fai.toml.
        let pkg = make_pkg_with_widget_type("check_ext_ok");
        let proj = temp_dir("check_ext_proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();

        let pkg_path = pkg.to_string_lossy();
        std::fs::write(
            proj.join("fai.toml"),
            format!(
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\nWidgetPkg = \"file://{}\"\n",
                pkg_path
            ),
        ).unwrap();

        // Entry file: imports Widget from external package.
        std::fs::write(
            proj.join("src").join("main.fai"),
            "use { Widget, makeWidget } from WidgetPkg\n\ndef main\n    @return Widget\ndo\n  makeWidget('hello')\nend\n",
        ).unwrap();
        // Sibling file: also uses the external type — triggers the multi-file path.
        std::fs::write(
            proj.join("src").join("helper.fai"),
            "use { Widget, makeWidget } from WidgetPkg\n\n# Helper.\ndef helperWidget\n    @return Widget\ndo\n  makeWidget('helper')\nend\n",
        ).unwrap();

        let entry = proj.join("src").join("main.fai");
        let result = try_check_single_file(&entry.to_string_lossy());
        assert!(
            result.is_ok(),
            "check with external dep type should succeed; got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_prepare_module_directory_does_not_load_external_deps() {
        // Documents why the old check_single_file path was broken:
        // prepare_module_directory loads only the given directory, not fai.toml
        // dependencies, so external package types are unknown.
        let pkg = make_pkg_with_widget_type("mod_dir_broken");
        let proj = temp_dir("mod_dir_broken_proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();

        let pkg_path = pkg.to_string_lossy();
        std::fs::write(
            proj.join("fai.toml"),
            format!(
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\nWidgetPkg = \"file://{}\"\n",
                pkg_path
            ),
        ).unwrap();
        std::fs::write(
            proj.join("src").join("main.fai"),
            "use { Widget, makeWidget } from WidgetPkg\n\ndef main\n    @return Widget\ndo\n  makeWidget('hello')\nend\n",
        ).unwrap();
        std::fs::write(
            proj.join("src").join("helper.fai"),
            "use { Widget, makeWidget } from WidgetPkg\n\n# Helper.\ndef helperWidget\n    @return Widget\ndo\n  makeWidget('helper')\nend\n",
        ).unwrap();

        // prepare_module_directory has no access to fai.toml — external types unknown.
        let src_dir = proj.join("src").to_string_lossy().into_owned();
        let prepared = fai_compiler::prepare_module_directory(&src_dir).unwrap();
        let mut checker = fai_checker::Checker::new();
        let result = run_checker(&mut checker, &prepared);
        assert!(
            result.is_err(),
            "prepare_module_directory without dep resolution should fail type check"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Widget") || err.contains("Unknown"),
            "error should mention the unknown type; got: {}",
            err
        );
    }

    #[test]
    fn test_run_project_check_catches_errors_in_multi_target_nested_src() {
        // Regression test: a multi-target project (fai.toml has
        // [project.<name>] sub-projects) with a nested-only src/ —
        // i.e. no top-level .fai files, only subdirectories like
        // `auth/`, `data/`, `pages/` — used to silently pass
        // `fai check`. The bug: step_check fell through to its
        // flat-library mode, which builds a synthetic entry from
        // top-level `use` lines. With no top-level files those lines
        // are empty, no modules get discovered, and check_with_modules
        // walks nothing.
        //
        // The fix: detect [project.<name>] sub-projects in
        // run_project_check and dispatch to per-target check, the
        // same way step_test already does for tests.
        let proj = temp_dir("multi_target_nested_check");
        std::fs::create_dir_all(proj.join("src/auth")).unwrap();
        std::fs::create_dir_all(proj.join("src/platforms/server")).unwrap();
        std::fs::write(
            proj.join("fai.toml"),
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\
             \n[project.server]\ntarget = \"native\"\nsource = \"src\"\n\
             main = \"src/platforms/server/main.fai\"\n\
             build_dir = \"build/server\"\n",
        )
        .unwrap();

        // Nested file with a deliberate doc-comment violation on a
        // public function. Doc comments are required language-wide
        // and `fai check` must surface this.
        std::fs::write(
            proj.join("src/auth/login.fai"),
            "def login\n    @return Bool\ndo\n  true\nend\n",
        )
        .unwrap();

        // Server entry that imports from auth.
        std::fs::write(
            proj.join("src/platforms/server/main.fai"),
            "use { login } from auth\n\n\
             def main\n    @return Void\ndo\n  let _ = login()\nend\n",
        )
        .unwrap();

        let result = run_project_check(&proj, "src");
        assert!(
            result.is_err(),
            "fai check should fail on doc-comment violation in a nested src/ \
             of a multi-target project, got Ok"
        );
        let (msg, _count) = result.unwrap_err();
        assert!(
            msg.contains("doc comment") && msg.contains("login"),
            "error should report the missing doc comment on `login`, got:\n{}",
            msg
        );
    }

    // ── print_usage ──────────────────────────────────────────────────

    #[test]
    fn test_print_usage_no_panic() {
        // Verifies print_usage runs without panic and covers those lines
        print_usage();
    }

    // ── resolve_default_entry_point ──────────────────────────────────
    //
    // Regression tests for the multi-target error path. `fai run` in a
    // fullstack project with both `[project.client]` and
    // `[project.server]` used to print a positional-argument hint
    // (`fai run client`) — we now require `--project NAME` for
    // consistency with the rest of the CLI. These tests exercise the
    // decision logic via `resolve_default_entry_point_at` so the
    // behaviour doesn't depend on the runtime cwd (which is shared
    // across parallel tests).

    #[test]
    fn test_resolve_default_entry_multi_target_returns_none() {
        // With 2+ sub-projects and no --project flag, the function
        // must return None so the caller exits with the usage hint.
        // A silent fallback (e.g. picking the first target
        // alphabetically) would be worse — the two targets have
        // different effects (server starts a listener, client bundles
        // WASM).
        let root = temp_dir("resolve_default_multi");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\n\
             name = \"Multi\"\n\
             version = \"0.1.0\"\n\
             \n\
             [project.client]\n\
             target = \"wasm-html\"\n\
             source = \"src/client\"\n\
             \n\
             [project.server]\n\
             target = \"native\"\n\
             source = \"src/server\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src/client")).unwrap();
        std::fs::create_dir_all(root.join("src/server")).unwrap();
        std::fs::write(
            root.join("src/client/main.fai"),
            "def main\n    @return Void\ndo\nend\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/server/main.fai"),
            "def main\n    @return Void\ndo\nend\n",
        )
        .unwrap();

        let result = resolve_default_entry_point_at(&root);
        assert!(
            result.is_none(),
            "multi-target project without --project should return None, got {:?}",
            result
        );
    }

    #[test]
    fn test_resolve_default_entry_single_sub_project_picks_it() {
        // Only one sub-project declared — treat it as the default.
        // This preserves the ergonomic case where a workspace grows
        // one target first, then adds a second (and becomes subject
        // to the multi-target rule above).
        let root = temp_dir("resolve_default_single");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\n\
             name = \"Single\"\n\
             version = \"0.1.0\"\n\
             \n\
             [project.server]\n\
             target = \"native\"\n\
             source = \"src/server\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src/server")).unwrap();
        std::fs::write(
            root.join("src/server/main.fai"),
            "def main\n    @return Void\ndo\nend\n",
        )
        .unwrap();

        let result = resolve_default_entry_point_at(&root);
        assert!(
            result.is_some(),
            "single sub-project should resolve to its main.fai"
        );
        assert!(
            result
                .as_deref()
                .unwrap_or("")
                .ends_with("src/server/main.fai"),
            "resolved path should point at src/server/main.fai, got {:?}",
            result
        );
    }

    #[test]
    fn test_resolve_default_entry_plain_project_uses_src_convention() {
        // A project with no sub-projects at all — legacy/plain
        // layout — still resolves via the `source_root = "src"`
        // convention. Nothing to do with multi-target, just making
        // sure the refactor didn't break the default path.
        let root = temp_dir("resolve_default_plain");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"Plain\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.fai"),
            "def main\n    @return Void\ndo\nend\n",
        )
        .unwrap();

        let result = resolve_default_entry_point_at(&root);
        assert!(
            result.is_some(),
            "plain project with src/main.fai should resolve"
        );
    }

    // ── Scaffold functions ───────────────────────────────────────────

    #[test]
    fn test_scaffold_main_contains_name() {
        let out = scaffold_main("myproject");
        assert!(out.contains("myproject"));
        assert!(out.contains("print("));
    }

    #[test]
    fn test_scaffold_fai_toml_structure() {
        let out = scaffold_fai_toml("myproject");
        assert!(out.contains("[project]"));
        assert!(out.contains("name = \"myproject\""));
        assert!(out.contains("version = \"0.1.0\""));
        assert!(out.contains("source_root = \"src\""));
        assert!(out.contains("[dependencies]"));
    }

    #[test]
    fn test_scaffold_readme_contains_name() {
        let out = scaffold_readme("myproject");
        assert!(out.contains("myproject"));
        assert!(out.contains("fai run"));
    }

    #[test]
    fn test_scaffold_language_md_has_sections() {
        let out = scaffold_language_md();
        assert!(out.contains("## Types"));
        assert!(out.contains("## Functions"));
        assert!(out.contains("## Variables"));
        assert!(out.contains("## Control Flow"));
        assert!(out.contains("## Modules and Imports"));
        assert!(out.contains("## Standard Library"));
        assert!(out.contains("## Testing"));
    }

    #[test]
    fn test_scaffold_claude_md_contains_name() {
        let out = scaffold_claude_md("myproject");
        assert!(out.contains("myproject"));
        assert!(out.contains("fai run"));
        assert!(out.contains("fai check"));
        assert!(out.contains("private:"));
        assert!(
            out.contains("one file per function")
                || out.contains("One file per function")
                || out.contains("one function per file")
                || out.contains("one-file-per-function")
        );
    }

    #[test]
    fn test_scaffold_agents_md_has_content() {
        let out = scaffold_agents_md();
        assert!(out.contains("fai run"));
        assert!(out.contains("fai check"));
        assert!(out.contains("private:"));
        assert!(out.contains("## File Structure Rules") || out.contains("## File Structure"));
    }

    // ── HTML loader generators ───────────────────────────────────────

    #[test]
    fn test_generate_html_loader_contains_filename() {
        let html = generate_html_loader("app.wasm");
        assert!(html.contains("app.wasm"));
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("<script>"));
        assert!(html.contains("_start_async"));
        assert!(html.contains("__fai_poll"));
        assert!(html.contains("__fai_task_result"));
        assert!(html.contains("__fai_resume_task"));
        assert!(html.contains("pumpAsync()"));
        assert!(html.contains("startFai()"));
    }

    // ── require_file_arg ─────────────────────────────────────────────

    #[test]
    fn test_require_file_arg_finds_path() {
        let args: Vec<String> = vec!["myfile.fai".to_string()];
        let result = require_file_arg(&args, "run");
        assert_eq!(result, "myfile.fai");
    }

    #[test]
    fn test_require_file_arg_skips_flags() {
        let args: Vec<String> = vec!["--wasm".to_string(), "myfile.fai".to_string()];
        let result = require_file_arg(&args, "run");
        assert_eq!(result, "myfile.fai");
    }

    // ── read_project_info ────────────────────────────────────────────

    #[test]
    fn test_read_project_info_no_dir() {
        let (name, version, _) = read_project_info(None);
        assert_eq!(name, "unknown");
        assert_eq!(version, "0.0.0");
    }

    #[test]
    fn test_read_project_info_with_toml() {
        let dir = temp_dir("proj_info");
        // Include a comment and unknown key to exercise lines 307 (unknown key → `_ => {}`)
        // and 309 (line with no `=` like a comment → split_once returns None)
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\n# a comment\nname = \"myapp\"\nversion = \"1.2.3\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        let (name, version, _) = read_project_info(Some(dir.to_str().unwrap()));
        assert_eq!(name, "myapp");
        assert_eq!(version, "1.2.3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_project_info_root_path() {
        // Covers line 290: the else branch when src_path.parent() is None
        // This happens when the path is "/" (root) which has no parent
        let (name, version, _) = read_project_info(Some("/"));
        // Should return defaults since /fai.toml doesn't exist (or isn't readable)
        assert_eq!(name, "unknown");
        assert_eq!(version, "0.0.0");
    }

    #[test]
    fn test_read_project_info_no_toml_file() {
        let dir = temp_dir("proj_info_nofile");
        let (name, version, _) = read_project_info(Some(dir.to_str().unwrap()));
        assert_eq!(name, "unknown");
        assert_eq!(version, "0.0.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── find_source_root ─────────────────────────────────────────────

    #[test]
    fn test_find_source_root_with_toml() {
        let dir = temp_dir("src_root");
        std::fs::write(dir.join("fai.toml"), "[project]\nsource_root = \"src\"\n").unwrap();
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("main.fai");
        std::fs::write(&file, SIMPLE_FAI).unwrap();

        let root = find_source_root(file.to_str().unwrap());
        assert!(root.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_source_root_no_toml() {
        let dir = temp_dir("src_root_none");
        let file = dir.join("main.fai");
        std::fs::write(&file, SIMPLE_FAI).unwrap();
        // No fai.toml in the directory tree (temp_dir is deep)
        // Result may be None or Some depending on filesystem — just verify no panic
        let _ = find_source_root(file.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_source_root_multi_section_toml() {
        // Covers the `if !in_project { continue; }` branch (line 1145) when
        // fai.toml has sections before [project]
        let dir = temp_dir("src_root_multi");
        std::fs::write(
            dir.join("fai.toml"),
            "[meta]\ndescription = \"test\"\n\n[project]\nsource_root = \"src\"\n",
        )
        .unwrap();
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("main.fai");
        std::fs::write(&file, SIMPLE_FAI).unwrap();

        let root = find_source_root(file.to_str().unwrap());
        assert!(root.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_project_info_multi_section_toml() {
        // Covers the `if !in_project { continue; }` branch in read_project_info
        let dir = temp_dir("proj_info_multi");
        std::fs::write(
            dir.join("fai.toml"),
            "[meta]\nauthors = [\"foo\"]\n\n[project]\nname = \"myapp\"\nversion = \"2.0.0\"\n",
        )
        .unwrap();
        let (name, version, _) = read_project_info(Some(dir.to_str().unwrap()));
        assert_eq!(name, "myapp");
        assert_eq!(version, "2.0.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_check ────────────────────────────────────────────────────

    #[test]
    fn test_cmd_check_valid_file() {
        let path = write_fai("cmd_check", SIMPLE_FAI);
        let args: Vec<String> = vec![path.clone()];
        cmd_check(&args); // should print "ok" and return normally
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    // ── cmd_fmt ──────────────────────────────────────────────────────

    #[test]
    fn test_cmd_fmt_already_formatted() {
        let path = write_fai("cmd_fmt", "let x = 42\n");
        let args: Vec<String> = vec![path.clone()];
        cmd_fmt(&args); // should print "already formatted"
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_fmt_check_mode() {
        let path = write_fai("cmd_fmt_check", "let x = 42\n");
        let args: Vec<String> = vec![path.clone(), "--check".to_string()];
        cmd_fmt(&args); // check mode — should print "ok"
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_fmt_formats_and_prints_path() {
        // Covers the "formatted <path>" loop (lines 497-499)
        let dir = temp_dir("cmd_fmt_formatted");
        let path = dir.join("test.fai");
        // Write without trailing newline — needs reformatting
        std::fs::write(&path, "let x = 42").unwrap();

        let args: Vec<String> = vec![path.to_str().unwrap().to_string()];
        cmd_fmt(&args); // should print "formatted <path>"

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "let x = 42\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_run ──────────────────────────────────────────────────────

    #[test]
    fn test_cmd_run_fai_file() {
        let path = write_fai("cmd_run", SIMPLE_FAI);
        let args: Vec<String> = vec![path.clone()];
        cmd_run(&args);
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_run_wasm_flag() {
        // Tests the --wasm JIT path in cmd_run
        let path = write_fai("cmd_run_wasm", SIMPLE_FAI);
        let args: Vec<String> = vec![path.clone(), "--wasm".to_string()];
        cmd_run(&args);
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_run_wasm_file() {
        // Tests running a pre-compiled .wasm file directly
        let dir = temp_dir("cmd_run_wasm_file");
        let fai_path = dir.join("prog.fai");
        let wasm_path = dir.join("prog.wasm");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();

        // First build the .wasm file
        let build_args: Vec<String> = vec![fai_path.to_str().unwrap().to_string()];
        cmd_build(&build_args);
        assert!(wasm_path.exists(), "wasm file must exist for this test");

        // Then run it directly
        let run_args: Vec<String> = vec![wasm_path.to_str().unwrap().to_string()];
        cmd_run(&run_args);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_test ─────────────────────────────────────────────────────

    // ── cmd_build ────────────────────────────────────────────────────

    #[test]
    fn test_cmd_build_produces_wasm() {
        let dir = temp_dir("cmd_build");
        let fai_path = dir.join("prog.fai");
        let wasm_path = dir.join("prog.wasm");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();

        let args: Vec<String> = vec![fai_path.to_str().unwrap().to_string()];
        cmd_build(&args);

        assert!(wasm_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_with_output_flag() {
        let dir = temp_dir("cmd_build_o");
        let fai_path = dir.join("prog.fai");
        let out_path = dir.join("out.wasm");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();

        let args: Vec<String> = vec![
            fai_path.to_str().unwrap().to_string(),
            "-o".to_string(),
            out_path.to_str().unwrap().to_string(),
        ];
        cmd_build(&args);

        assert!(out_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_non_fai_extension() {
        // No fai.toml + non-`.fai` extension. The naming policy strips
        // the extension via Path::file_stem, so prog.txt builds to
        // prog.wasm. (Previously the policy preserved the full filename
        // and produced prog.txt.wasm — that was an artefact of the old
        // strip-suffix branch and didn't compose with the new
        // project-name-driven naming.)
        let dir = temp_dir("cmd_build_txt");
        let txt_path = dir.join("prog.txt");
        std::fs::write(&txt_path, SIMPLE_FAI).unwrap();

        let args: Vec<String> = vec![txt_path.to_str().unwrap().to_string()];
        cmd_build(&args);

        assert!(dir.join("prog.wasm").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generate_runtime_js_exposes_view_event_bridges() {
        // The forui view layer wires DOM events through forai's std.events:
        // the generated inline `onclick`/`oninput`/`onkeydown` handlers call
        // `handleEvent` / `handleInputEvent` / `handleSubmitEvent`, which emit
        // `view:click`, `view:input`, `view:submit` topics. Forui subscribes
        // to those topics and runs the registered closures.
        let js = generate_runtime_js("prog.wasm");
        assert!(js.contains("function handleEvent"));
        assert!(js.contains("function handleInputEvent"));
        assert!(js.contains("function handleSubmitEvent"));
        assert!(
            js.contains("'view:click'"),
            "handleEvent should emit on view:click:\n{}",
            js
        );
        assert!(
            js.contains("'view:input'"),
            "handleInputEvent should emit on view:input:\n{}",
            js
        );
        assert!(
            js.contains("'view:submit'"),
            "handleSubmitEvent should emit on view:submit:\n{}",
            js
        );
        assert!(
            js.contains("function faiEmitHostEvent")
                && js.contains("finally{faiHostRelease(dataVal)}")
                && js.contains("handleEvent(id){faiEmitHostEvent('view:click',jsToWasm({id:id}))}")
                && js.contains("handleInputEvent(id,value){faiEmitHostEvent('view:input',jsToWasm({id:id,value:value}))}")
                && js.contains("handleSubmitEvent(id){faiEmitHostEvent('view:submit',jsToWasm({id:id}))}"),
            "browser host-created event payloads must be released after dispatch:\n{}",
            js
        );
        assert!(
            js.contains("hostLive")
                && js.contains("if(s.host)faiLeak.hostLive--")
                && js.contains("return instance.exports.__live_objects.value+faiLeak.hostAllocs")
                && js.contains("window.__fai_leak_snapshot"),
            "browser leak diagnostics must report live host allocations while normalizing __live_objects for host-created allocations:\n{}",
            js
        );
        // The event_* env imports must be live, not stubs — std.events
        // is implemented host-side, including in the browser.
        assert!(js.contains("faiEventRegistry"));
        assert!(js.contains("faiInvokeClosure"));
        assert!(js.contains("_start_async"));
        assert!(js.contains("__fai_poll"));
        assert!(js.contains("__fai_task_result"));
        assert!(js.contains("__fai_resume_task"));
        assert!(js.contains("pumpAsync()"));
        assert!(js.contains("startFai()"));
    }

    #[test]
    fn test_generate_runtime_js_exposes_ownership_diagnostics() {
        let js = generate_runtime_js("prog.wasm");

        assert!(js.contains("function faiInstallOwnershipSitesFromWasm"));
        assert!(js.contains("meta.ownership_sites"));
        assert!(js.contains("faiOwnership.sites"));
        assert!(js.contains("faiOwnership.history"));
        assert!(js.contains("function faiOwnershipSiteLabel"));
        assert!(js.contains("function faiOwnershipAuxLabel"));
        assert!(js.contains("function faiOwnershipGroupSummary"));
        assert!(js.contains("groups=faiOwnershipGroupSummary(a)"));
        assert!(js.contains("out+='\\n  groups:'"));
        assert!(js.contains("faiInstallOwnershipSitesFromWasm(b);return WebAssembly.instantiate"));
        assert!(
            !js.contains("pos=end}catch"),
            "ownership site parser must close the try block before catch:\n{}",
            js
        );
        assert!(js.contains("window.__fai_assert_ownership"));
        assert!(js.contains("window.__fai_dump_ownership"));
    }

    #[test]
    fn test_generate_runtime_js_exposes_async_host_op_http_bridge() {
        let js = generate_runtime_js("prog.wasm");

        assert!(js.contains("var __faiHostOpResults={}"));
        assert!(js.contains("function readHostOpArgs"));
        assert!(js.contains("function hostOpBegin"));
        assert!(js.contains("function hostOpResult"));
        assert!(js.contains("method={1:'GET',2:'POST',3:'PUT',4:'PATCH',5:'DELETE'}[opKind]"));
        assert!(js.contains("if(opKind===8||opKind===10){done(jsToWasm(false));return}"));
        assert!(js.contains("if(opKind===9){done(jsToWasm([]));return}"));
        assert!(js.contains("if(opKind===12){done(jsToWasm(-1));return}"));
        assert!(js.contains("if(opKind>=11&&opKind<=15){done(NULL_VAL);return}"));
        assert!(js.contains("fetch(url,opts)"));
        assert!(
            js.contains("host_op_begin:function(taskId,opKind,count,argsPtr){hostOpBegin(taskId,opKind,count,argsPtr,faiServiceScheduler)}"),
            "env.host_op_begin should delegate to the async host-op bridge:\n{}",
            js
        );
        assert!(
            js.contains("host_op_result:function(taskId){return hostOpResult(taskId)}"),
            "env.host_op_result should read the host-op completion map:\n{}",
            js
        );
    }

    #[test]
    fn test_generate_runtime_js_is_valid_javascript() {
        let js = generate_runtime_js("prog.wasm");
        let dir = temp_dir("runtime_js_syntax");
        let path = dir.join("fai-runtime.js");
        std::fs::write(&path, js).unwrap();

        let out = std::process::Command::new("node")
            .arg("--check")
            .arg(&path)
            .output()
            .expect("node is required to syntax-check generated fai-runtime.js");
        assert!(
            out.status.success(),
            "node --check failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_with_html_flag() {
        let dir = temp_dir("cmd_build_html");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let fai_path = src.join("prog.fai");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"Test\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();

        let args: Vec<String> = vec![fai_path.to_str().unwrap().to_string(), "--html".to_string()];
        cmd_build(&args);

        let public = dir.join("public");
        assert!(public.join("Test.wasm").exists());
        assert!(public.join("index.html").exists());
        assert!(public.join("fai-runtime.js").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_with_target_wasm_html() {
        // Same behaviour as --html, but declared in fai.toml via
        // `target = "wasm-html"`. Plan 99 Phase 2.1.
        let dir = temp_dir("cmd_build_target_wasm_html");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let fai_path = src.join("prog.fai");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"Test\"\nversion = \"0.1.0\"\nsource_root = \"src\"\ntarget = \"wasm-html\"\n",
        ).unwrap();

        // No --html flag — target alone drives the html bundle.
        let args = vec![fai_path.to_str().unwrap().to_string()];
        cmd_build(&args);

        let public = dir.join("public");
        assert!(public.join("Test.wasm").exists());
        assert!(public.join("index.html").exists());
        assert!(public.join("fai-runtime.js").exists());
        assert!(public.join("forui.css").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generate_forui_css_includes_textarea_defaults() {
        let css = generate_forui_css();
        assert!(css.contains(".fai-textarea"));
        assert!(css.contains("resize:vertical"));
    }

    #[test]
    fn test_multi_target_browser_build_honors_instrumentation_env() {
        let dir = temp_dir("multi_target_browser_instrumented");
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"InstrumentedApp\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
             [project.web]\ntarget = \"wasm-html\"\nsource = \"src/web\"\nmain = \"src/web/main.fai\"\nbuild_dir = \"build/web\"\n\n\
             [project.server]\ntarget = \"wasm\"\nsource = \"src/server\"\nmain = \"src/server/main.fai\"\nbuild_dir = \"build/server\"\nrequired_targets = [\"web\"]\n",
        )
        .unwrap();
        let web_src = dir.join("src/web");
        let server_src = dir.join("src/server");
        std::fs::create_dir_all(&web_src).unwrap();
        std::fs::create_dir_all(&server_src).unwrap();
        std::fs::write(web_src.join("main.fai"), SIMPLE_FAI).unwrap();
        std::fs::write(server_src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        std::env::set_var("FAI_CHECK_LEAKS", "1");
        std::env::set_var("FAI_OWNERSHIP_CHECK", "1");
        cmd_build(&["server".to_string()]);
        std::env::remove_var("FAI_OWNERSHIP_CHECK");
        std::env::remove_var("FAI_CHECK_LEAKS");
        std::env::set_current_dir(&prev).unwrap();

        let wasm = std::fs::read(dir.join("build/web/web.wasm")).unwrap();
        assert!(bytes_contain(&wasm, b"__fai_alloc_event"));
        assert!(bytes_contain(&wasm, b"__fai_free_event"));
        assert!(bytes_contain(&wasm, b"__fai_ownership_event"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_schema_excludes_unreachable_remote_defs() {
        let dir = temp_dir("cmd_build_rpc_reachable_schema");
        let src = dir.join("src");
        let forui_pkg = dir.join("forui");
        std::fs::create_dir_all(src.join("platforms/server")).unwrap();
        std::fs::create_dir_all(src.join("data/tasks")).unwrap();
        std::fs::create_dir_all(src.join("data/admin")).unwrap();
        std::fs::create_dir_all(forui_pkg.join("src/rpc")).unwrap();

        std::fs::write(
            forui_pkg.join("fai.toml"),
            "[project]\nname = \"Forui\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(
            forui_pkg.join("src/rpc/main.fai"),
            concat!(
                "use std.http.server\n\n",
                "# Handles generated RPC requests.\n",
                "def handleRpcRequest\n",
                "    @param request HttpRequest\n",
                "    @param specJson String\n",
                "    @param specHash String\n",
                "    @param dispatch (String, String, Dictionary) -> String\n",
                "    @return HttpResponse\n",
                "do\n",
                "  server.json(200, '{}')\n",
                "end\n\n",
                "# Stub auth gate: always allows.\n",
                "def rpcAuthCheck\n",
                "    @param policy String\n",
                "    @param authorizerName String\n",
                "    @param ctx Dictionary\n",
                "    @param argsJson String\n",
                "    @return String\n",
                "do\n",
                "  ''\n",
                "end\n\n",
                "# Stub args parser: never parses.\n",
                "def rpcArgsOrNull\n",
                "    @param argsJson String\n",
                "    @return Unknown\n",
                "do\n",
                "  null\n",
                "end\n",
            ),
        )
        .unwrap();

        std::fs::write(
            dir.join("fai.toml"),
            format!(
                "[project]\nname = \"RpcReachable\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\nForui = \"file://{}\"\n",
                forui_pkg.display()
            ),
        )
        .unwrap();
        let server_main = src.join("platforms/server/main.fai");
        std::fs::write(
            &server_main,
            concat!(
                "use std.http.server\n",
                "use { getTasks } from data.tasks\n\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "  let r = server.router()\n",
                "  addRpcRoutes(r)\n",
                "end\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("data/tasks/main.fai"),
            concat!(
                "# Gets reachable tasks.\n",
                "remote def getTasks\n",
                "    @auth session\n",
                "    @return String[]\n",
                "do\n",
                "  []\n",
                "end\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("data/admin/main.fai"),
            concat!(
                "# Dangerous endpoint that must stay unexposed.\n",
                "remote def deleteEverything\n",
                "    @auth session\n",
                "    @return String\n",
                "do\n",
                "  'nope'\n",
                "end\n",
            ),
        )
        .unwrap();

        let out_path = dir.join("build/server/main.wasm");
        std::fs::create_dir_all(out_path.parent().unwrap()).unwrap();
        let args: Vec<String> = vec![
            server_main.to_string_lossy().into_owned(),
            "-o".to_string(),
            out_path.to_string_lossy().into_owned(),
        ];
        cmd_build(&args);

        let schema = std::fs::read_to_string(dir.join("build/server/schema.json"))
            .expect("server build should write schema.json");
        assert!(
            schema.contains("\"key\": \"data.tasks.getTasks\""),
            "schema should expose imported remote def. Got:\n{}",
            schema
        );
        assert!(
            !schema.contains("deleteEverything"),
            "schema should not expose unimported remote def. Got:\n{}",
            schema
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_remote_target_ignores_unreachable_async_helpers() {
        let dir = temp_dir("cmd_build_remote_dead_async_helper");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("platforms/web")).unwrap();
        std::fs::create_dir_all(src.join("platforms/server")).unwrap();
        std::fs::create_dir_all(src.join("data/people")).unwrap();

        std::fs::write(
            dir.join("fai.toml"),
            concat!(
                "[project]\n",
                "name = \"RemoteReachability\"\n",
                "version = \"0.1.0\"\n",
                "source_root = \"src\"\n",
                "\n",
                "[project.web]\n",
                "target = \"wasm-html\"\n",
                "source = \"src\"\n",
                "main = \"src/platforms/web/main.fai\"\n",
                "build_dir = \"build/web\"\n",
                "\n",
                "[project.server]\n",
                "target = \"wasm\"\n",
                "source = \"src\"\n",
                "main = \"src/platforms/server/main.fai\"\n",
                "build_dir = \"build/server\"\n",
                "rpc_server = true\n",
                "required_targets = [\"web\"]\n",
                "\n",
                "[project.web.dependencies.server.remote.dev]\n",
                "url = \"http://localhost:3040\"\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("platforms/web/main.fai"),
            concat!(
                "use { updatePerson } from data.people\n\n",
                "def main\n",
                "    @return String\n",
                "do\n",
                "  updatePerson(1, 'A')\n",
                "end\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("platforms/server/main.fai"),
            concat!(
                "use { updatePerson } from data.people\n\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "  let _ = updatePerson(1, 'A')\n",
                "end\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("data/people/updatePerson.fai"),
            concat!(
                "# Updates a person.\n",
                "remote def updatePerson\n",
                "    @param id Int\n",
                "    @param name String\n",
                "    @auth session\n",
                "    @return String\n",
                "do\n",
                "  name\n",
                "end\n\n",
                "# Unused async helper.\n",
                "def unusedAsyncHelper\n",
                "    @return Void\n",
                "do\n",
                "  for i in 0..3\n",
                "    sleep(1)\n",
                "  end\n",
                "end\n",
            ),
        )
        .unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&["server".to_string()]);
        std::env::set_current_dir(&prev).unwrap();

        assert!(dir.join("build/web/web.wasm").exists());
        assert!(dir.join("build/server/server.wasm").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_remote_target_rewrites_reachable_remote_body() {
        let dir = temp_dir("cmd_build_remote_rewrites_body");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("platforms/web")).unwrap();
        std::fs::create_dir_all(src.join("data/people")).unwrap();

        std::fs::write(
            dir.join("fai.toml"),
            concat!(
                "[project]\n",
                "name = \"RemoteRewriteBody\"\n",
                "version = \"0.1.0\"\n",
                "source_root = \"src\"\n",
                "\n",
                "[project.web]\n",
                "target = \"wasm-html\"\n",
                "source = \"src\"\n",
                "main = \"src/platforms/web/main.fai\"\n",
                "build_dir = \"build/web\"\n",
                "\n",
                "[project.web.dependencies.server.remote.dev]\n",
                "url = \"http://localhost:3040\"\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("platforms/web/main.fai"),
            concat!(
                "use { updatePerson } from data.people\n\n",
                "def main\n",
                "    @return String\n",
                "do\n",
                "  updatePerson(1, 'A')\n",
                "end\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("data/people/updatePerson.fai"),
            concat!(
                "# Updates a person.\n",
                "remote def updatePerson\n",
                "    @param id Int\n",
                "    @param name String\n",
                "    @auth session\n",
                "    @return String\n",
                "do\n",
                "  updatePersonInternal(id, name)\n",
                "end\n\n",
                "# Server-only implementation reached by the real remote body.\n",
                "def updatePersonInternal\n",
                "    @param id Int\n",
                "    @param name String\n",
                "    @return String\n",
                "do\n",
                "  for i in 0..3\n",
                "    sleep(1)\n",
                "  end\n",
                "  name\n",
                "end\n",
            ),
        )
        .unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&["web".to_string()]);
        std::env::set_current_dir(&prev).unwrap();

        assert!(dir.join("build/web/web.wasm").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rpc_test_stub_is_private() {
        let mut content = concat!(
            "use std.http.server\n\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let r = server.router()\n",
            "  addRpcRoutes(r)\n",
            "end\n",
        )
        .to_string();
        inject_rpc_test_stub(&mut content);
        let parsed = fai_parser::parse(&content).expect("stub should parse");
        let stub = parsed
            .statements
            .iter()
            .find_map(|stmt| match stmt {
                fai_parser::ast::Statement::Function(fd) if fd.name == "addRpcRoutes" => Some(fd),
                _ => None,
            })
            .expect("stub should be injected");
        assert!(stub.is_private, "test stub should not require coverage");
    }

    #[test]
    fn test_read_project_info_parses_target() {
        let dir = temp_dir("proj_info_target");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"T\"\nversion = \"0.1.0\"\ntarget = \"wasm-html\"\n",
        )
        .unwrap();
        let info = read_project_info_full(Some(dir.to_str().unwrap()));
        assert_eq!(info.target, Some(BuildTarget::WasmHtml));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_project_info_parses_workspace_members() {
        let dir = temp_dir("proj_info_workspace");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[workspace]\nmembers = [\"shared\", \"server\", \"client\"]\n",
        )
        .unwrap();
        let info = read_project_info_full(Some(dir.to_str().unwrap()));
        assert_eq!(info.workspace_members, vec!["shared", "server", "client"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_project_info_parses_remote_interface() {
        let dir = temp_dir("proj_info_remote_iface");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"S\"\nversion = \"0.1.0\"\n\n[remote-interface]\nexpose = true\n",
        )
        .unwrap();
        let info = read_project_info_full(Some(dir.to_str().unwrap()));
        assert!(info.interface_expose);
        assert!(info.interface_from.is_none());
        let _ = std::fs::remove_dir_all(&dir);

        let dir = temp_dir("proj_info_remote_from");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"C\"\nversion = \"0.1.0\"\n\n[remote-interface]\nfrom = \"SharedPkg\"\n",
        ).unwrap();
        let info = read_project_info_full(Some(dir.to_str().unwrap()));
        assert!(!info.interface_expose);
        assert_eq!(info.interface_from.as_deref(), Some("SharedPkg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Plan 101: Sub-project and remote dependency parsing ──────

    #[test]
    fn test_parse_sub_projects() {
        let info = parse_project_info(
            "[project]\nname = \"TodoApp\"\nversion = \"0.1.0\"\n\n\
             [project.client]\ntarget = \"wasm-html\"\nsource = \"client/src\"\nbuild_dir = \"client/public\"\n\n\
             [project.server]\ntarget = \"native\"\nsource = \"server/src\"\n\n\
             [project.shared]\nsource = \"shared/src\"\n"
        );
        assert_eq!(info.name, "TodoApp");
        assert_eq!(info.sub_projects.len(), 3);

        let client = &info.sub_projects["client"];
        assert_eq!(client.target, Some(BuildTarget::WasmHtml));
        assert_eq!(client.source.as_deref(), Some("client/src"));
        assert_eq!(client.build_dir.as_deref(), Some("client/public"));

        let server = &info.sub_projects["server"];
        assert_eq!(server.target, Some(BuildTarget::Native));
        assert_eq!(server.source.as_deref(), Some("server/src"));
        assert!(server.build_dir.is_none());

        let shared = &info.sub_projects["shared"];
        assert!(shared.target.is_none());
        assert_eq!(shared.source.as_deref(), Some("shared/src"));
    }

    // ── Plan 132: [secrets] manifest parsing ─────────────────────

    #[test]
    fn test_parse_secrets_manifest() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\n\n\
             [secrets]\nbackend = \"dotenvx\"\n\
             STRIPE_KEY = { required = true }\n\
             OPENAI_API_KEY = { required = true, targets = [\"server\", \"worker\"] }\n\
             SLACK_BOT_TOKEN = {}\n\n\
             [secrets.aws]\nregion = \"us-east-1\"\nprefix = \"brain/prod/\"\n",
        );
        let secrets = info.secrets.expect("secrets section parsed");
        assert_eq!(secrets.backend, "dotenvx");
        assert_eq!(secrets.declarations.len(), 3);

        let stripe = &secrets.declarations[0];
        assert_eq!(stripe.name, "STRIPE_KEY");
        assert!(stripe.required);
        assert!(stripe.targets.is_empty());

        let openai = &secrets.declarations[1];
        assert!(openai.required);
        assert_eq!(openai.targets, vec!["server", "worker"]);

        let slack = &secrets.declarations[2];
        assert!(!slack.required);
        assert!(slack.targets.is_empty());

        let aws = &secrets.backend_options["aws"];
        assert_eq!(aws["region"], "us-east-1");
        assert_eq!(aws["prefix"], "brain/prod/");
    }

    #[test]
    fn test_parse_secrets_backend_defaults_to_env() {
        let info = parse_project_info("[secrets]\nAPI_KEY = { required = true }\n");
        let secrets = info.secrets.expect("secrets section parsed");
        assert_eq!(secrets.backend, "env");
        assert_eq!(secrets.declarations.len(), 1);
    }

    #[test]
    fn test_parse_secrets_absent_is_none() {
        let info = parse_project_info("[project]\nname = \"App\"\n");
        assert!(info.secrets.is_none());
    }

    #[test]
    fn test_secrets_declarations_for_target() {
        let info = parse_project_info(
            "[secrets]\n\
             ALL = { required = true }\n\
             SERVER_ONLY = { required = true, targets = [\"server\"] }\n",
        );
        let secrets = info.secrets.unwrap();
        let server = secrets.declarations_for_target(Some("server"));
        assert_eq!(server.len(), 2);
        let client = secrets.declarations_for_target(Some("client"));
        assert_eq!(client.len(), 1);
        assert_eq!(client[0].name, "ALL");
        // Loose/single-project runs (no sub-project target) validate only
        // untargeted declarations.
        let loose = secrets.declarations_for_target(None);
        assert_eq!(loose.len(), 1);
    }

    #[test]
    fn test_parse_sub_projects_dont_clobber_root() {
        // Sub-project sections shouldn't overwrite root [project] fields
        let info = parse_project_info(
            "[project]\nname = \"App\"\nversion = \"2.0.0\"\ntarget = \"wasm\"\n\n\
             [project.client]\ntarget = \"wasm-html\"\nsource = \"client/src\"\n",
        );
        assert_eq!(info.name, "App");
        assert_eq!(info.version, "2.0.0");
        assert_eq!(info.target, Some(BuildTarget::Wasm));
        assert_eq!(info.sub_projects.len(), 1);
        assert_eq!(
            info.sub_projects["client"].target,
            Some(BuildTarget::WasmHtml)
        );
    }

    #[test]
    fn test_parse_remote_dependency_config() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
             [project.client]\ntarget = \"wasm-html\"\nsource = \"client/src\"\n\n\
             [project.client.dependencies.shared.remote.dev]\nurl = \"http://localhost:3040\"\n\n\
             [project.client.dependencies.shared.remote.prod]\nurl = \"https://api.myapp.com\"\n",
        );
        let client = &info.sub_projects["client"];
        assert_eq!(client.remote_deps.len(), 1);
        let shared_remote = &client.remote_deps["shared"];
        assert_eq!(shared_remote.len(), 2);
        assert_eq!(shared_remote["dev"].url, "http://localhost:3040");
        assert_eq!(shared_remote["prod"].url, "https://api.myapp.com");
    }

    #[test]
    fn test_parse_multiple_remote_deps() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
             [project.client]\nsource = \"src\"\n\n\
             [project.client.dependencies.auth.remote.dev]\nurl = \"http://localhost:4000\"\n\n\
             [project.client.dependencies.tasks.remote.dev]\nurl = \"http://localhost:4001\"\n",
        );
        let client = &info.sub_projects["client"];
        assert_eq!(client.remote_deps.len(), 2);
        assert_eq!(
            client.remote_deps["auth"]["dev"].url,
            "http://localhost:4000"
        );
        assert_eq!(
            client.remote_deps["tasks"]["dev"].url,
            "http://localhost:4001"
        );
    }

    #[test]
    fn test_parse_single_project_no_sub_projects() {
        // Single-project toml should still work with zero sub-projects
        let info = parse_project_info(
            "[project]\nname = \"MyTool\"\nversion = \"1.0.0\"\ntarget = \"native\"\n",
        );
        assert_eq!(info.name, "MyTool");
        assert_eq!(info.target, Some(BuildTarget::Native));
        assert!(info.sub_projects.is_empty());
    }

    #[test]
    fn test_parse_backwards_compat_workspace_members() {
        // Old workspace format should still work
        let info =
            parse_project_info("[workspace]\nmembers = [\"shared\", \"server\", \"client\"]\n");
        assert_eq!(info.workspace_members, vec!["shared", "server", "client"]);
        assert!(info.sub_projects.is_empty());
    }

    // ── Plan 101: Project root, entry point, target resolution ──

    #[test]
    fn test_find_project_root() {
        let dir = temp_dir("proj_root");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("fai.toml"), "[project]\nname = \"X\"\n").unwrap();
        // From the src subdirectory, should find the parent
        let found = find_project_root(&dir.join("src"));
        assert_eq!(found.unwrap(), dir);
        // From the root itself
        let found = find_project_root(&dir);
        assert_eq!(found.unwrap(), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_entry_point_main() {
        let dir = temp_dir("entry_main");
        std::fs::create_dir_all(dir.join("server/src")).unwrap();
        std::fs::write(
            dir.join("server/src/main.fai"),
            "def main\n    @return Void\ndo\n  print('hi')\nend\n",
        )
        .unwrap();
        std::fs::write(dir.join("server/src/other.fai"), "").unwrap();
        let entry = resolve_entry_point(&dir, "server/src");
        assert_eq!(entry.unwrap().file_name().unwrap(), "main.fai");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_entry_point_first_fai() {
        let dir = temp_dir("entry_first");
        std::fs::create_dir_all(dir.join("client/src")).unwrap();
        std::fs::write(dir.join("client/src/todoclient.fai"), "").unwrap();
        let entry = resolve_entry_point(&dir, "client/src");
        assert!(entry.is_some());
        assert!(entry.unwrap().extension().unwrap() == "fai");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_entry_point_missing_dir() {
        let dir = temp_dir("entry_missing");
        std::fs::create_dir_all(&dir).unwrap();
        let entry = resolve_entry_point(&dir, "nonexistent/src");
        assert!(entry.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_select_targets_by_name() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\n\n\
             [project.client]\nsource = \"client/src\"\n\n\
             [project.server]\nsource = \"server/src\"\n",
        );
        let targets = select_targets(&info, Some("client"));
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "client");
    }

    #[test]
    fn test_select_targets_all() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\n\n\
             [project.client]\nsource = \"client/src\"\n\n\
             [project.server]\nsource = \"server/src\"\n",
        );
        let targets = select_targets(&info, None);
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn test_select_targets_unknown_name() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\n\n\
             [project.client]\nsource = \"client/src\"\n",
        );
        let targets = select_targets(&info, Some("nope"));
        assert!(targets.is_empty());
    }

    #[test]
    fn test_select_targets_single_project() {
        let info = parse_project_info("[project]\nname = \"Tool\"\ntarget = \"native\"\n");
        let targets = select_targets(&info, None);
        assert!(
            targets.is_empty(),
            "single project returns empty — handled separately"
        );
    }

    #[test]
    fn test_pack_native_binary_trailer_layout() {
        // Unit test: pack_native_binary produces [forai][wasm][magic][len]
        // and read_embedded_wasm on that file extracts the wasm back.
        // Plan 99 Phase 3.
        let dir = temp_dir("pack_native_layout");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("embedded");
        let wasm = b"\x00asm\x01\x00\x00\x00fake-wasm-body"; // minimal wasm magic + filler
        pack_native_binary(wasm, &out).expect("pack should succeed");

        let bytes = std::fs::read(&out).unwrap();
        assert!(
            bytes.len() > 16 + wasm.len(),
            "output should include forai + wasm + trailer"
        );

        // Trailer: last 16 bytes = magic + u64 length.
        let n = bytes.len();
        assert_eq!(&bytes[n - 16..n - 8], NATIVE_TRAILER_MAGIC);
        let len = u64::from_le_bytes(bytes[n - 8..n].try_into().unwrap());
        assert_eq!(len as usize, wasm.len());

        // Wasm payload right before the trailer.
        let payload_start = n - 16 - wasm.len();
        assert_eq!(&bytes[payload_start..n - 16], wasm);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_native_build_produces_runnable_binary() {
        // End-to-end: `cmd_build` with target="native" writes a
        // self-extracting binary; spawning it should run the program
        // and emit the expected print output. Plan 99 Phase 3.
        //
        // Requires a built forai binary at <workspace>/target/debug/forai.
        // Skipped when the binary isn't present to avoid spurious
        // failures in environments that haven't built it yet.
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest.parent().unwrap().parent().unwrap();
        let forai_bin = workspace.join("target").join("debug").join("forai");
        if !forai_bin.exists() {
            eprintln!(
                "skipping native-build e2e test: {} missing. `cargo build` first.",
                forai_bin.display()
            );
            return;
        }

        let dir = temp_dir("native_e2e");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"NativeTest\"\nversion = \"0.1.0\"\nsource_root = \"src\"\ntarget = \"native\"\n",
        ).unwrap();
        let fai_path = src.join("main.fai");
        let src_code = concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print('native binary says hi')\n",
            "end\n",
        );
        std::fs::write(&fai_path, src_code).unwrap();

        // Point pack_native_binary at the real forai binary rather
        // than the cargo test harness (which is what current_exe
        // would otherwise return).
        std::env::set_var("FORAI_SELF_BINARY", &forai_bin);
        cmd_build(&[fai_path.to_str().unwrap().to_string()]);
        std::env::remove_var("FORAI_SELF_BINARY");

        // Native binary is named after [project].name ("NativeTest"),
        // not the source file's stem.
        let native = src.join("NativeTest");
        assert!(
            native.exists(),
            "native binary not produced at {}",
            native.display()
        );

        // Execute it and check stdout.
        let out = std::process::Command::new(&native)
            .output()
            .expect("failed to spawn native binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "native binary exited nonzero. stderr: {}",
            stderr
        );
        assert!(
            stdout.contains("native binary says hi"),
            "native binary stdout missing expected output. stdout: {}",
            stdout
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Regression: the plain forai binary (no embedded wasm) must
    // dispatch to normal CLI, not try to self-extract. Covered by
    // main.rs's test_help_flag + test_run_prints_output — both spawn
    // the forai binary without a trailer and expect CLI behaviour.

    #[test]
    fn test_cmd_build_workspace_iterates_members() {
        // Workspace with two members, each a minimal package.
        // `forai build` invoked in the workspace root (via cwd) should
        // build both. Plan 99 Phase 2.2.
        let dir = temp_dir("cmd_build_workspace");
        std::fs::create_dir_all(&dir).unwrap();

        // Workspace root toml listing the two members.
        std::fs::write(
            dir.join("fai.toml"),
            "[workspace]\nmembers = [\"pkg_a\", \"pkg_b\"]\n",
        )
        .unwrap();

        // Member A: entry point at src/main.fai
        let a_src = dir.join("pkg_a").join("src");
        std::fs::create_dir_all(&a_src).unwrap();
        std::fs::write(
            dir.join("pkg_a").join("fai.toml"),
            "[project]\nname = \"PkgA\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(a_src.join("main.fai"), SIMPLE_FAI).unwrap();

        // Member B: entry point at src/pkgb.fai (named convention).
        let b_src = dir.join("pkg_b").join("src");
        std::fs::create_dir_all(&b_src).unwrap();
        std::fs::write(
            dir.join("pkg_b").join("fai.toml"),
            "[project]\nname = \"PkgB\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(b_src.join("pkgb.fai"), SIMPLE_FAI).unwrap();

        // Change cwd into the workspace root and invoke `forai build`
        // with no file arg.
        let _guard = cwd_test_lock();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[]);
        std::env::set_current_dir(&prev_cwd).unwrap();

        // Both members should have produced a .wasm next to their
        // source file (default output location when no build_dir set).
        // Filename comes from each member's [project].name, not from
        // the source file's stem.
        assert!(
            a_src.join("PkgA.wasm").exists(),
            "pkg_a PkgA.wasm should exist, dir contents: {:?}",
            std::fs::read_dir(&a_src).unwrap().collect::<Vec<_>>()
        );
        assert!(
            b_src.join("PkgB.wasm").exists(),
            "pkg_b PkgB.wasm should exist, dir contents: {:?}",
            std::fs::read_dir(&b_src).unwrap().collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── lift_target_name_positional ──────────────────────────────────

    /// Helper that writes a fai.toml with two sub-projects (`client`,
    /// `server`) at `dir`. Used by the lift + scoping tests below.
    fn write_sub_project_toml(dir: &std::path::Path) {
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"app\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
             [project.client]\ntarget = \"wasm\"\nsource = \"src/client\"\nmain = \"src/client/main.fai\"\n\n\
             [project.server]\ntarget = \"wasm\"\nsource = \"src/server\"\nmain = \"src/server/main.fai\"\n",
        ).unwrap();
    }

    #[test]
    fn test_lift_target_name_positional_recognises_sub_project() {
        // `fai build client` with a fai.toml that has [project.client]
        // should lift `client` to the project flag and remove it from
        // args so step_fmt/check/test don't try to open "client" as a file.
        let dir = temp_dir("lift_target_name_matches");
        write_sub_project_toml(&dir);

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let (args, project) = lift_target_name_positional(vec!["client".to_string()], None);
        std::env::set_current_dir(&prev).unwrap();

        assert!(
            args.is_empty(),
            "positional should be stripped, got {:?}",
            args
        );
        assert_eq!(project.as_deref(), Some("client"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lift_target_name_positional_ignores_file_paths() {
        // `fai build src/main.fai` should NOT be lifted — it's a file
        // path, not a target name.
        let dir = temp_dir("lift_target_name_filepath");
        write_sub_project_toml(&dir);

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let (args, project) = lift_target_name_positional(vec!["src/main.fai".to_string()], None);
        std::env::set_current_dir(&prev).unwrap();

        assert_eq!(args, vec!["src/main.fai".to_string()]);
        assert!(project.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lift_target_name_positional_ignores_unknown_name() {
        // `fai build notatarget` — no match in sub_projects, leave alone.
        // cmd_build will then fall through to the file-open path which
        // produces the normal "no such file" error.
        let dir = temp_dir("lift_target_name_unknown");
        write_sub_project_toml(&dir);

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let (args, project) = lift_target_name_positional(vec!["notatarget".to_string()], None);
        std::env::set_current_dir(&prev).unwrap();

        assert_eq!(args, vec!["notatarget".to_string()]);
        assert!(project.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lift_target_name_positional_leaves_explicit_project_flag_alone() {
        // When --project is already set, positional (even if it matches
        // a sub-project) must pass through untouched. This keeps the
        // user's explicit flag authoritative.
        let dir = temp_dir("lift_target_name_explicit");
        write_sub_project_toml(&dir);

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let (args, project) =
            lift_target_name_positional(vec!["client".to_string()], Some("server".to_string()));
        std::env::set_current_dir(&prev).unwrap();

        assert_eq!(args, vec!["client".to_string()]);
        assert_eq!(project.as_deref(), Some("server"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_build target scoping ────────────────────────────────────

    /// End-to-end test: with two sub-projects, `cmd_build` scoped to
    /// one target (via --project or positional) should only produce
    /// that target's build output.
    ///
    /// Uses `target = "wasm"` so no forai binary / HTML renderer is
    /// needed — the build step just emits a .wasm file.
    fn build_two_sub_projects_and_check(
        tag: &str,
        args: Vec<String>,
        expect_client: bool,
        expect_server: bool,
    ) {
        let dir = temp_dir(tag);
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"app\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
             [project.client]\ntarget = \"wasm\"\nsource = \"src/client\"\nmain = \"src/client/main.fai\"\nbuild_dir = \"build/client\"\n\n\
             [project.server]\ntarget = \"wasm\"\nsource = \"src/server\"\nmain = \"src/server/main.fai\"\nbuild_dir = \"build/server\"\n",
        ).unwrap();
        let client_src = dir.join("src/client");
        let server_src = dir.join("src/server");
        std::fs::create_dir_all(&client_src).unwrap();
        std::fs::create_dir_all(&server_src).unwrap();
        std::fs::write(client_src.join("main.fai"), SIMPLE_FAI).unwrap();
        std::fs::write(server_src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&args);
        std::env::set_current_dir(&prev).unwrap();

        let client_out = dir.join("build/client/client.wasm");
        let server_out = dir.join("build/server/server.wasm");
        assert_eq!(
            client_out.exists(),
            expect_client,
            "client wasm present={} but expected={} for args {:?}",
            client_out.exists(),
            expect_client,
            args
        );
        assert_eq!(
            server_out.exists(),
            expect_server,
            "server wasm present={} but expected={} for args {:?}",
            server_out.exists(),
            expect_server,
            args
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── scan_module_for_tests_and_publics ────────────────────────────

    const SIBLING_ENTRY_FAI: &str = concat!(
        "use { App } from client\n\n",
        "def main\n",
        "    @return Void\n",
        "do\n",
        "  print(App())\n",
        "end\n",
    );

    const SIBLING_APP_FAI: &str = concat!(
        "# The app shell.\n",
        "def App\n",
        "    @return String\n",
        "do\n",
        "  'hi'\n",
        "end\n",
    );

    #[test]
    fn test_scan_module_picks_up_sibling_public_fns() {
        // Regression for the partners bug: entry file has no public
        // functions (just `main`), but a sibling file defines `App`.
        // scan_module must report App so the early-return path fails
        // with "missing test block" instead of reporting "no public
        // functions to test".
        let dir = temp_dir("scan_module_sibling_publics");
        let entry = dir.join("main.fai");
        std::fs::write(&entry, SIBLING_ENTRY_FAI).unwrap();
        std::fs::write(dir.join("app.fai"), SIBLING_APP_FAI).unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (has_tests, publics) = scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert!(!has_tests, "neither file has a test block");
        assert_eq!(publics, vec!["App".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_module_detects_sibling_test_block() {
        // When a sibling file has a test block (but the entry file
        // doesn't), has_test_blocks must still come back true so the
        // module proceeds to the VM instead of short-circuiting.
        let dir = temp_dir("scan_module_sibling_tests");
        let entry = dir.join("main.fai");
        std::fs::write(&entry, SIBLING_ENTRY_FAI).unwrap();
        // Use a raw literal for the test-block variant so we don't
        // fight with the macro above.
        std::fs::write(
            dir.join("app.fai"),
            "# The app shell.\ndef App\n    @return String\ndo\n  'hi'\nend\n\ntest App\nit 'returns hi'\n  assert.equals(App(), 'hi')\nend\nend\n",
        ).unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (has_tests, publics) = scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert!(has_tests, "sibling file has a test block");
        assert_eq!(publics, vec!["App".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_module_entry_only_still_works() {
        // Single-file module (no siblings) — behaviour should match
        // what the old entry-only scan produced.
        let dir = temp_dir("scan_module_entry_only");
        let entry = dir.join("solo.fai");
        std::fs::write(
            &entry,
            "# A greeting.\ndef greet\n    @return String\ndo\n  'hi'\nend\n\ndef main\n    @return Void\ndo\n  print(greet())\nend\n",
        ).unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (has_tests, publics) = scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert!(!has_tests);
        assert_eq!(publics, vec!["greet".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_module_walks_nested_directories() {
        // Regression: the partners client has `pages/`, `components/`,
        // and `state/` subdirs under src/client, each holding public
        // functions. A non-recursive walker would miss all of them and
        // report `[ok] test — no public functions`. The recursive walker
        // must pick up public fns no matter how deep they are nested.
        let dir = temp_dir("scan_module_nested_dirs");
        let entry = dir.join("main.fai");
        std::fs::write(&entry, SIBLING_ENTRY_FAI).unwrap();
        std::fs::write(dir.join("app.fai"), SIBLING_APP_FAI).unwrap();

        let components = dir.join("components");
        std::fs::create_dir_all(&components).unwrap();
        std::fs::write(
            components.join("button.fai"),
            "# A button.\ndef Button\n    @return String\ndo\n  'click'\nend\n",
        )
        .unwrap();

        let pages = dir.join("pages");
        let pages_team = pages.join("team");
        std::fs::create_dir_all(&pages_team).unwrap();
        std::fs::write(
            pages.join("home.fai"),
            "# Home page.\ndef HomePage\n    @return String\ndo\n  'home'\nend\n",
        )
        .unwrap();
        // Two levels deep — must still be found.
        std::fs::write(
            pages_team.join("detail.fai"),
            "# Team detail.\ndef TeamDetail\n    @return String\ndo\n  'team'\nend\n",
        )
        .unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (has_tests, publics) = scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert!(!has_tests);
        assert_eq!(
            publics,
            vec![
                "App".to_string(),
                "Button".to_string(),
                "HomePage".to_string(),
                "TeamDetail".to_string(),
            ],
            "expected all public fns across nested module dirs"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_fai_files_recursive_returns_all_depths() {
        let dir = temp_dir("collect_recursive");
        std::fs::write(dir.join("a.fai"), "").unwrap();
        let nested = dir.join("sub").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("b.fai"), "").unwrap();
        std::fs::write(dir.join("sub").join("c.fai"), "").unwrap();
        // Non-.fai files must be skipped.
        std::fs::write(dir.join("notes.md"), "").unwrap();

        let files = collect_fai_files_recursive(&dir);
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                std::path::Path::new(f)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "a.fai".to_string(),
                "c.fai".to_string(),
                "b.fai".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_module_deduplicates_public_fns() {
        // If two files accidentally define the same public name (or the
        // entry is listed twice), the returned list must not repeat it.
        // This is a defensive check — the compiler would reject the
        // actual duplicate, but scan shouldn't inflate the missing-test
        // count before we even try to compile.
        let dir = temp_dir("scan_module_dedup");
        let entry = dir.join("main.fai");
        std::fs::write(
            &entry,
            "# A greeting.\ndef greet\n    @return String\ndo\n  'hi'\nend\n\ndef main\n    @return Void\ndo\n  print(greet())\nend\n",
        ).unwrap();
        // Second file repeats `greet` — scan_module must dedupe.
        std::fs::write(
            dir.join("other.fai"),
            "# Another greeting.\ndef greet\n    @return String\ndo\n  'yo'\nend\n",
        )
        .unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (_has_tests, publics) =
            scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert_eq!(publics, vec!["greet".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_project_flag_scopes_to_one_target() {
        build_two_sub_projects_and_check(
            "cmd_build_project_flag",
            vec!["--project".to_string(), "client".to_string()],
            true,
            false,
        );
    }

    #[test]
    fn test_cmd_build_positional_target_name_scopes_to_one_target() {
        build_two_sub_projects_and_check(
            "cmd_build_positional_target",
            vec!["client".to_string()],
            true,
            false,
        );
    }

    #[test]
    fn test_cmd_build_no_args_builds_all_targets() {
        build_two_sub_projects_and_check("cmd_build_all", vec![], true, true);
    }

    #[test]
    fn test_cmd_build_html_write_warning() {
        // Covers the `Err(e) => eprintln!("warning...")` path (line 231)
        // by making the html output path a directory so the write fails gracefully
        let dir = temp_dir("cmd_build_html_warn");
        let fai_path = dir.join("prog.fai");
        let wasm_path = dir.join("prog.wasm");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();
        // Create a directory named "prog.html" — fs::write to a dir fails
        std::fs::create_dir_all(dir.join("prog.html")).unwrap();

        let args: Vec<String> = vec![
            fai_path.to_str().unwrap().to_string(),
            "-o".to_string(),
            wasm_path.to_str().unwrap().to_string(),
            "--html".to_string(),
        ];
        cmd_build(&args); // html write fails with warning, wasm still written

        assert!(wasm_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_interface ────────────────────────────────────────────────

    #[test]
    fn test_cmd_interface_outputs_json() {
        let path = write_fai("cmd_iface", INTERFACE_FAI);
        let args: Vec<String> = vec![path.clone()];
        cmd_interface(&args); // prints JSON to stdout
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_interface_with_output_file() {
        let dir = temp_dir("cmd_iface_o");
        let fai_path = dir.join("prog.fai");
        let out_path = dir.join("interface.json");
        std::fs::write(&fai_path, INTERFACE_FAI).unwrap();

        let args: Vec<String> = vec![
            fai_path.to_str().unwrap().to_string(),
            "-o".to_string(),
            out_path.to_str().unwrap().to_string(),
        ];
        cmd_interface(&args);

        assert!(out_path.exists());
        let json = std::fs::read_to_string(&out_path).unwrap();
        assert!(json.contains("\"functions\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_new ──────────────────────────────────────────────────────

    #[test]
    fn test_cmd_new_creates_project() {
        let base = temp_dir("cmd_new_base");
        let project_path = base.join("myproject");

        // cmd_new creates the project at the given path
        let args: Vec<String> = vec![project_path.to_str().unwrap().to_string()];
        cmd_new(&args);

        assert!(project_path.join("src").join("main.fai").exists());
        assert!(project_path.join("fai.toml").exists());
        assert!(project_path.join("README.md").exists());
        assert!(project_path.join("language.md").exists());
        assert!(project_path.join("CLAUDE.md").exists());
        assert!(project_path.join("AGENTS.md").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_cmd_new_with_local_template() {
        let base = temp_dir("cmd_new_local_tpl");
        let tpl = base.join("tpl");

        // Stand up a tiny template fixture
        std::fs::create_dir_all(tpl.join("src/pages")).unwrap();
        std::fs::write(
            tpl.join("fai.toml"),
            "[project]\nname = \"TplName\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(
            tpl.join("src/pages/home.fai"),
            "def HomePage\n  @return Void\ndo\nend\n",
        )
        .unwrap();
        std::fs::write(tpl.join("README.md"), "# tpl readme\n").unwrap();

        let project_path = base.join("scaffolded-app");
        let args: Vec<String> = vec![
            project_path.to_str().unwrap().to_string(),
            "--template".to_string(),
            tpl.to_str().unwrap().to_string(),
        ];
        cmd_new(&args);

        // Template files copied verbatim
        assert!(project_path.join("src/pages/home.fai").exists());
        assert!(project_path.join("README.md").exists());
        // Project name substituted in fai.toml
        let toml = std::fs::read_to_string(project_path.join("fai.toml")).unwrap();
        assert!(
            toml.contains("name = \"scaffolded-app\""),
            "fai.toml should carry the new project name, got:\n{}",
            toml
        );
        assert!(
            !toml.contains("TplName"),
            "old name should be gone from fai.toml, got:\n{}",
            toml
        );

        // Meta files (language reference, AI guidance, MCP config) are
        // overlaid by `fai new` regardless of template — they're
        // language-level concerns, not project-shape.
        assert!(project_path.join("CLAUDE.md").exists());
        assert!(project_path.join("AGENTS.md").exists());
        assert!(project_path.join("language.md").exists());
        assert!(project_path.join(".mcp.json").exists());
        assert!(project_path.join(".codex/config.toml").exists());

        // The auto-overlaid CLAUDE.md should reference the new project
        // name, not the template's source name.
        let claude = std::fs::read_to_string(project_path.join("CLAUDE.md")).unwrap();
        assert!(
            claude.starts_with("# scaffolded-app\n"),
            "CLAUDE.md heading should use the new project name, got:\n{}",
            &claude[..claude.len().min(80)]
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_cmd_new_appends_template_meta_to_scaffold() {
        // A template that ships its own CLAUDE.md / AGENTS.md (forui-
        // specific guidance, say) gets appended below the language-
        // level scaffold so both are visible. Language-level rules
        // (doc comments, testing) come first; project-specific
        // guidance follows under a separator.
        let base = temp_dir("cmd_new_meta_append");
        let tpl = base.join("tpl");
        std::fs::create_dir_all(tpl.join("src")).unwrap();
        std::fs::write(tpl.join("fai.toml"), "[project]\nname = \"X\"\n").unwrap();
        std::fs::write(
            tpl.join("src/main.fai"),
            "def main\n  @return Void\ndo\nend\n",
        )
        .unwrap();
        std::fs::write(
            tpl.join("CLAUDE.md"),
            "# Custom guidance\n\nTemplate-owned.\n",
        )
        .unwrap();
        std::fs::write(
            tpl.join("AGENTS.md"),
            "# Custom AGENTS\n\nTemplate-agents.\n",
        )
        .unwrap();

        let project_path = base.join("app");
        cmd_new(&[
            project_path.to_str().unwrap().to_string(),
            "--template".to_string(),
            tpl.to_str().unwrap().to_string(),
        ]);

        let claude = std::fs::read_to_string(project_path.join("CLAUDE.md")).unwrap();
        assert!(
            claude.contains("Template-owned"),
            "template-supplied CLAUDE.md content should be preserved, got:\n{}",
            claude
        );
        assert!(
            claude.contains("Project-specific guidance"),
            "merged CLAUDE.md should carry the separator header, got:\n{}",
            claude
        );
        assert!(
            claude.find("Template-owned").unwrap()
                > claude.find("Project-specific guidance").unwrap(),
            "scaffold should come first, template second:\n{}",
            claude
        );

        let agents = std::fs::read_to_string(project_path.join("AGENTS.md")).unwrap();
        assert!(
            agents.contains("Template-agents") && agents.contains("doc comment required"),
            "merged AGENTS.md should carry both scaffold (doc-comment rule) and template content, got:\n{}",
            agents
        );

        // Meta files the template *didn't* ship still get filled in.
        assert!(project_path.join("language.md").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn overlay_meta_writes_files_when_absent() {
        let base = temp_dir("overlay_writes");
        let dir = base.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        overlay_meta_files(&dir, "my-app");
        assert!(dir.join("language.md").exists());
        assert!(dir.join("CLAUDE.md").exists());
        assert!(dir.join("AGENTS.md").exists());
        assert!(dir.join(".mcp.json").exists());
        assert!(dir.join(".codex/config.toml").exists());
    }

    #[test]
    fn overlay_meta_appends_template_content_to_scaffold() {
        // Existing CLAUDE.md / AGENTS.md gets appended below the
        // scaffold rather than replaced. Other meta files (e.g.
        // .mcp.json) still use last-write-wins.
        let base = temp_dir("overlay_appends");
        let dir = base.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "OWNED").unwrap();
        overlay_meta_files(&dir, "my-app");
        let merged = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(
            merged.contains("OWNED"),
            "template content kept: {}",
            merged
        );
        assert!(
            merged.contains("Project-specific guidance"),
            "separator header present: {}",
            merged
        );
        assert!(
            merged.find("OWNED").unwrap() > merged.find("Project-specific").unwrap(),
            "scaffold first, template second: {}",
            merged
        );
    }

    #[test]
    fn overlay_meta_preserves_non_md_files() {
        // .mcp.json, .codex/config.toml, language.md keep last-write-wins
        // semantics — appending doesn't make sense for structured files.
        let base = temp_dir("overlay_preserves_structured");
        let dir = base.join("p");
        std::fs::create_dir_all(dir.join(".codex")).unwrap();
        std::fs::write(dir.join(".mcp.json"), "{\"custom\":true}").unwrap();
        overlay_meta_files(&dir, "my-app");
        assert_eq!(
            std::fs::read_to_string(dir.join(".mcp.json")).unwrap(),
            "{\"custom\":true}"
        );
    }

    #[test]
    fn overlay_meta_interpolates_project_name() {
        let base = temp_dir("overlay_name");
        let dir = base.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        overlay_meta_files(&dir, "fancy-app");
        let claude = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(claude.starts_with("# fancy-app\n"));
    }

    #[test]
    fn overlay_meta_creates_codex_dir_if_missing() {
        let base = temp_dir("overlay_codex");
        let dir = base.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        // .codex doesn't exist yet
        overlay_meta_files(&dir, "p");
        assert!(dir.join(".codex").is_dir());
        assert!(dir.join(".codex/config.toml").exists());
    }

    // ── artifact_filename / sub_project_output_path helpers ─────────

    #[test]
    fn artifact_filename_uses_project_name_when_set() {
        assert_eq!(
            artifact_filename("MySuperApp", "/x/y/main.fai"),
            "MySuperApp.wasm"
        );
        assert_eq!(
            artifact_filename("Forui", "/anywhere/entry.fai"),
            "Forui.wasm"
        );
    }

    #[test]
    fn artifact_filename_falls_back_to_source_stem_when_name_is_default_unknown() {
        // The parser fills `name` with "unknown" when `name = "..."`
        // is missing from [project]. That sentinel must trigger the
        // source-stem fallback so we don't ship `unknown.wasm`.
        assert_eq!(
            artifact_filename("unknown", "/x/y/myscratch.fai"),
            "myscratch.wasm"
        );
    }

    #[test]
    fn artifact_filename_falls_back_when_name_is_empty() {
        assert_eq!(artifact_filename("", "/x/main.fai"), "main.wasm");
    }

    #[test]
    fn artifact_filename_strips_extension_in_fallback() {
        // The fallback is Path::file_stem-based, so any extension is
        // stripped — not just `.fai`.
        assert_eq!(artifact_filename("", "/x/scratch.txt"), "scratch.wasm");
    }

    #[test]
    fn sub_project_output_path_uses_build_dir_when_set() {
        let tmp = temp_dir("subproj_path_with_bd");
        let entry = tmp.join("src/web/main.fai");
        let sub = SubProject {
            build_dir: Some("build/web".to_string()),
            ..SubProject::default()
        };
        let out = sub_project_output_path(&sub, &tmp, &entry, "web");
        assert_eq!(out, tmp.join("build/web/web.wasm").to_string_lossy());
        assert!(tmp.join("build/web").is_dir(), "out dir should be created");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sub_project_output_path_falls_back_to_entry_dir_when_no_build_dir() {
        let tmp = temp_dir("subproj_path_no_bd");
        let entry_dir = tmp.join("src/server");
        std::fs::create_dir_all(&entry_dir).unwrap();
        let entry = entry_dir.join("main.fai");
        let sub = SubProject::default();
        let out = sub_project_output_path(&sub, &tmp, &entry, "server");
        assert_eq!(out, entry_dir.join("server.wasm").to_string_lossy());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── Build artifact naming ────────────────────────────────────────
    // The .wasm filename derives from the project's `name` field, not
    // from the source file's stem. So `name = "MyApp"` always builds
    // to `MyApp.wasm` regardless of whether the entry is main.fai,
    // entry.fai, or anything else. For multi-project files, each
    // sub-project's artifact uses the sub-project key (`web`, `server`,
    // …). Source-stem naming remains as the fallback for ad-hoc
    // builds with no fai.toml or with the default `"unknown"` name.

    #[test]
    fn test_build_uses_project_name_for_wasm_artifact() {
        let dir = temp_dir("build_name_single_wasm");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"MySuperApp\"\nversion = \"0.1.0\"\nsource_root = \"src\"\nbuild_dir = \"out\"\n",
        ).unwrap();
        std::fs::write(src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[src.join("main.fai").to_string_lossy().into_owned()]);
        std::env::set_current_dir(&prev).unwrap();

        let named = dir.join("out/MySuperApp.wasm");
        let stem_named = dir.join("out/main.wasm");
        assert!(
            named.exists(),
            "expected MySuperApp.wasm (project name), out dir: {:?}",
            std::fs::read_dir(dir.join("out")).ok().map(|d| d
                .filter_map(|e| e.ok().map(|x| x.file_name()))
                .collect::<Vec<_>>())
        );
        assert!(
            !stem_named.exists(),
            "main.wasm should NOT exist — naming should come from project name"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_html_uses_project_name() {
        let dir = temp_dir("build_name_single_html");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"BrowserApp\"\nversion = \"0.1.0\"\nsource_root = \"src\"\ntarget = \"wasm-html\"\nbuild_dir = \"public\"\n",
        ).unwrap();
        std::fs::write(src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[
            src.join("main.fai").to_string_lossy().into_owned(),
            "--html".to_string(),
        ]);
        std::env::set_current_dir(&prev).unwrap();

        assert!(
            dir.join("public/BrowserApp.wasm").exists(),
            "wasm-html build should write BrowserApp.wasm"
        );
        assert!(
            !dir.join("public/main.wasm").exists(),
            "wasm-html build should NOT write main.wasm"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_falls_back_to_source_stem_without_fai_toml() {
        // A loose .fai file with no fai.toml has no project name to
        // use. The naming policy falls back to the source stem so
        // ad-hoc `forai build foo.fai` keeps working.
        let dir = temp_dir("build_name_no_toml");
        let path = dir.join("scratch.fai");
        std::fs::write(&path, SIMPLE_FAI).unwrap();

        cmd_build(&[path.to_string_lossy().into_owned()]);

        assert!(
            dir.join("scratch.wasm").exists(),
            "no fai.toml should fall back to <source-stem>.wasm"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_falls_back_when_name_is_default_unknown() {
        // fai.toml exists but doesn't set `name` (parser leaves it as
        // the default "unknown"). The fallback should still kick in —
        // we don't want files called `unknown.wasm`.
        let dir = temp_dir("build_name_default_unknown");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nversion = \"0.1.0\"\nsource_root = \"src\"\nbuild_dir = \"out\"\n",
        )
        .unwrap();
        std::fs::write(src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[src.join("main.fai").to_string_lossy().into_owned()]);
        std::env::set_current_dir(&prev).unwrap();

        assert!(
            dir.join("out/main.wasm").exists(),
            "missing name should fall back to <source-stem>.wasm"
        );
        assert!(
            !dir.join("out/unknown.wasm").exists(),
            "should not produce unknown.wasm from the default name"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_sub_project_uses_sub_project_key_as_artifact_name() {
        // Multi-project: `[project.web]` and `[project.server]` should
        // produce `web.wasm` and `server.wasm` regardless of each
        // sub-project's main.fai stem.
        let dir = temp_dir("build_name_multi");
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"AppShell\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
             [project.web]\ntarget = \"wasm\"\nsource = \"src/web\"\nmain = \"src/web/main.fai\"\nbuild_dir = \"build/web\"\n\n\
             [project.server]\ntarget = \"wasm\"\nsource = \"src/server\"\nmain = \"src/server/main.fai\"\nbuild_dir = \"build/server\"\n",
        ).unwrap();
        let web_src = dir.join("src/web");
        let server_src = dir.join("src/server");
        std::fs::create_dir_all(&web_src).unwrap();
        std::fs::create_dir_all(&server_src).unwrap();
        std::fs::write(web_src.join("main.fai"), SIMPLE_FAI).unwrap();
        std::fs::write(server_src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[]);
        std::env::set_current_dir(&prev).unwrap();

        assert!(
            dir.join("build/web/web.wasm").exists(),
            "sub-project 'web' should produce web.wasm"
        );
        assert!(
            dir.join("build/server/server.wasm").exists(),
            "sub-project 'server' should produce server.wasm"
        );
        assert!(
            !dir.join("build/web/main.wasm").exists(),
            "sub-project 'web' should NOT produce main.wasm"
        );
        assert!(
            !dir.join("build/server/main.wasm").exists(),
            "sub-project 'server' should NOT produce main.wasm"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
