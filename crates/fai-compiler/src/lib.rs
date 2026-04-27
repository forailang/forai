pub mod ast;
pub mod module;

/// Back-compat alias. The bytecode compiler in `compiler.rs` was
/// deleted in Plan 94 Phase H; `DiscoveredModule` moved to its
/// own small file. External callers that still write
/// `fai_compiler::compiler::DiscoveredModule` keep compiling.
pub mod compiler {
    pub use crate::module::DiscoveredModule;
}
pub mod ffi_config;
pub mod limit_synths;
pub mod native_bridge;
pub mod regression_fixtures;

use ast::Program;
use module::DiscoveredModule;
use std::path::{Path, PathBuf};

/// A parsed and module-discovered program ready for type checking and compilation.
pub struct PreparedProgram {
    pub serde_ast: Program,
    pub modules: Vec<DiscoveredModule>,
    /// Set by the `_for_tests` prepare variants. When true, the compiler
    /// preserves `test` blocks in imported modules (they'd otherwise be
    /// stripped so production builds don't run other files' tests) and
    /// keeps `include_in_coverage = true` for every function in every
    /// module — so coverage can demand a test for *every* function
    /// across the whole target, not just the entry file.
    pub is_test: bool,
}

/// Phase 1 of compilation: parse + discover modules → PreparedProgram.
pub fn prepare_source(source: &str, source_root: Option<&str>) -> Result<PreparedProgram, String> {
    prepare_source_with_synthetic(source, source_root, Vec::new())
}

/// Prepare all .fai files in a directory as a single module for type-checking.
/// Useful for library projects that have no single entry-point file.
/// The returned PreparedProgram has an empty main AST and the directory's
/// files loaded as a module named `__module__`, so the checker can validate
/// all cross-file references within the library.
pub fn prepare_module_directory(dir_path: &str) -> Result<PreparedProgram, String> {
    let module = load_module_directory("__module__", dir_path, None, None, false)?;
    let empty_ast = fai_parser::parse("")?;
    let serde_ast = native_bridge::convert_program(&empty_ast);
    Ok(PreparedProgram {
        serde_ast,
        modules: vec![module],
        is_test: false,
    })
}

/// Test-mode variant of [`prepare_module_directory`].
///
/// Loads every `.fai` file in `dir_path` into a single module with
/// `is_test = true`, so test blocks are preserved and every public
/// function flagged for coverage. Used by `fai test` for library
/// projects that don't have a single entry-point file — the runner
/// walks the source root as one module and runs every test it finds
/// in one wasm pass.
///
/// Crucially, this also resolves transitive module dependencies. The
/// loaded `__module__` typically references both nested submodule
/// directories (e.g. `data/tasks/`) and external packages declared in
/// `fai.toml` (e.g. `Forsqlite`). To pull those in, the function
/// builds a synthetic entry source containing the union of every
/// `use` statement found across the source root's files and runs the
/// standard `discover_modules` walk against it. The directory itself
/// is then attached as `__module__` so its function and test
/// statements participate in codegen.
pub fn prepare_module_directory_for_tests(dir_path: &str) -> Result<PreparedProgram, String> {
    // Collect every distinct `use` line across the directory's files
    // so the synthetic entry mirrors what an entry-point file would
    // see if the user had written one. Module-mate symbols don't need
    // explicit `use` — they're visible through `__module__` directly.
    let entries = std::fs::read_dir(dir_path)
        .map_err(|e| format!("cannot read module directory '{}': {}", dir_path, e))?;
    let mut fai_files: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) == Some("fai") {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    fai_files.sort();

    let mut entry_source = String::new();
    let mut seen_use_lines = std::collections::HashSet::new();
    for file_path in &fai_files {
        let content = std::fs::read_to_string(file_path)
            .map_err(|e| format!("cannot read '{}': {}", file_path.display(), e))?;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("use ") && seen_use_lines.insert(trimmed.to_string()) {
                entry_source.push_str(trimmed);
                entry_source.push('\n');
            }
        }
    }

    // Run the standard test-prepare path against the synthetic entry
    // so external packages, std modules, and nested submodules all
    // come along through `discover_modules`. Then attach the loaded
    // directory as a same-source-root module so its definitions and
    // test blocks are part of the program.
    let mut prepared = prepare_source_with_synthetic_and_entry_for_tests(
        &entry_source,
        Some(dir_path),
        Vec::new(),
        None,
    )?;
    let module = load_module_directory("__module__", dir_path, None, None, true)?;
    prepared.modules.insert(0, module);
    Ok(prepared)
}

/// Prepare source with additional synthetic modules that exist only
/// in memory (no files on disk). Each entry is (module_name, source_code).
/// Other files can `use { X } from ModuleName` to access them.
pub fn prepare_source_with_synthetic(
    source: &str,
    source_root: Option<&str>,
    synthetic_modules: Vec<(String, String)>,
) -> Result<PreparedProgram, String> {
    prepare_source_with_synthetic_and_entry(source, source_root, synthetic_modules, None)
}

/// Same as `prepare_source_with_synthetic` plus an `entry_path` option
/// that excludes that specific file from any module's on-disk file list.
///
/// Why this matters: when the entry file lives inside a directory that
/// other modules can import (e.g. `src/server/main.fai` while another
/// module does `use { ... } from server`), the on-disk module loader
/// would pick up `main.fai` as part of the `server` module and compile
/// it a second time — this time from the raw source on disk, without
/// any injected dispatch the CLI applied to the entry. That second
/// compilation then fails on names the injection introduced
/// (`addRpcRoutes`, etc.). Excluding the entry file from module loads
/// prevents the double-compilation and the cascade it causes.
pub fn prepare_source_with_synthetic_and_entry(
    source: &str,
    source_root: Option<&str>,
    synthetic_modules: Vec<(String, String)>,
    entry_path: Option<&str>,
) -> Result<PreparedProgram, String> {
    prepare_source_impl(source, source_root, synthetic_modules, entry_path, false)
}

/// Test-mode variant: like `prepare_source_with_synthetic_and_entry`
/// but marks the resulting `PreparedProgram` so the compiler preserves
/// `test` blocks in imported modules and includes every module function
/// in coverage. Used by the `fai test` / per-target `fai build` test
/// step so a missing test anywhere in the target tree fails the build.
pub fn prepare_source_with_synthetic_and_entry_for_tests(
    source: &str,
    source_root: Option<&str>,
    synthetic_modules: Vec<(String, String)>,
    entry_path: Option<&str>,
) -> Result<PreparedProgram, String> {
    prepare_source_impl(source, source_root, synthetic_modules, entry_path, true)
}

fn prepare_source_impl(
    source: &str,
    source_root: Option<&str>,
    synthetic_modules: Vec<(String, String)>,
    entry_path: Option<&str>,
    is_test: bool,
) -> Result<PreparedProgram, String> {
    let native_ast = fai_parser::parse(source)?;

    // Collect synthetic module names so discover_modules skips them
    let synthetic_names: std::collections::HashSet<String> = synthetic_modules
        .iter()
        .map(|(name, _)| name.clone())
        .collect();

    // Canonicalise the entry path so comparisons against loaded-file
    // paths survive `./` or redundant slashes. When a module's directory
    // on disk contains this file, the loader reuses the already-parsed
    // `native_ast` instead of re-reading the raw file — the raw file
    // doesn't include the peer-hash / RPC-dispatch injections the CLI
    // applied to the entry, so a disk re-parse would fail on names
    // those injections introduce (e.g. `addRpcRoutes`).
    let entry_canonical: Option<std::path::PathBuf> =
        entry_path.and_then(|p| std::fs::canonicalize(p).ok());
    let entry_ref = entry_canonical.as_ref().map(|p| p.as_path());

    let mut modules = if let Some(root) = source_root {
        discover_modules(
            &native_ast.statements,
            root,
            &synthetic_names,
            entry_ref,
            Some(&native_ast),
            is_test,
        )?
    } else {
        Vec::new()
    };

    // Add synthetic modules (generated code like RPC proxies)
    for (name, src) in &synthetic_modules {
        let mod_ast = fai_parser::parse(src)?;
        let serde_program = native_bridge::convert_program(&mod_ast);
        modules.push(compiler::DiscoveredModule {
            name: name.clone(),
            statements: serde_program.statements,
            private_names: Vec::new(),
        });
    }

    let serde_ast = native_bridge::convert_program(&native_ast);
    Ok(PreparedProgram {
        serde_ast,
        modules,
        is_test,
    })
}

fn resolve_project_root_from_source_root(source_root: &str) -> PathBuf {
    let src_path = Path::new(source_root);
    let src_toml = src_path.join("fai.toml");
    if src_toml.exists() {
        return src_path.to_path_buf();
    }

    if let Some(parent) = src_path.parent() {
        let parent_toml = parent.join("fai.toml");
        if parent_toml.exists() {
            return parent.to_path_buf();
        }
    }

    src_path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_load_package_deps_tags_sibling_subprojects() {
        // [dependencies] entries are real external packages;
        // [project.<name>] entries are siblings in the same workspace.
        // The is_sibling flag drives how lowercase refs resolve from
        // inside each package's source.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf = std::env::temp_dir().join(format!("fai-siblings-{}", nonce));
        fs::create_dir_all(root.join("src/client")).unwrap();
        fs::create_dir_all(root.join("src/server")).unwrap();
        // External package lives outside the workspace.
        let ext_root = std::env::temp_dir().join(format!("fai-ext-pkg-{}", nonce));
        fs::create_dir_all(ext_root.join("src")).unwrap();
        fs::write(
            ext_root.join("fai.toml"),
            "[project]\nname = \"ExtPkg\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        // Workspace fai.toml with two sub-projects + the external dep.
        fs::write(
            root.join("fai.toml"),
            format!(
                "[project]\nname = \"WS\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
                 [project.client]\nsource = \"src/client\"\n\n\
                 [project.server]\nsource = \"src/server\"\n\n\
                 [dependencies]\n\"file://{}\" = \"0.1.0\"\n",
                ext_root.display(),
            ),
        )
        .unwrap();

        let src_root = root.join("src").to_string_lossy().into_owned();
        let packages = load_package_deps(&src_root);

        let client = packages
            .get("Client")
            .expect("Client sub-project should be registered");
        assert!(
            client.is_sibling,
            "[project.client] should be tagged sibling"
        );
        let server = packages
            .get("Server")
            .expect("Server sub-project should be registered");
        assert!(
            server.is_sibling,
            "[project.server] should be tagged sibling"
        );
        let ext = packages
            .get("ExtPkg")
            .expect("external dep should be registered");
        assert!(
            !ext.is_sibling,
            "[dependencies] entries are external packages, not siblings"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&ext_root);
    }

    #[test]
    fn test_discover_modules_cross_sibling_import_resolves_to_workspace_root() {
        // Regression for the partners `circular import: client → client`
        // bug. When the server's main.fai does `use { App } from
        // client`, and separately something pulls Server as a package
        // (e.g. SSR / RPC proxy), the discovery walk re-entered
        // src/server/ with package_prefix=Some("Server") and tried to
        // resolve `client` against src/server/client/ — which doesn't
        // exist. The sibling-aware branch now redirects those lowercase
        // refs back to the workspace source root so both `client` and
        // `server` coexist as siblings that can import each other.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf = std::env::temp_dir().join(format!("fai-xsibling-{}", nonce));
        fs::create_dir_all(root.join("src/client")).unwrap();
        fs::create_dir_all(root.join("src/server")).unwrap();
        fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"XS\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
             [project.client]\nsource = \"src/client\"\n\n\
             [project.server]\nsource = \"src/server\"\n",
        )
        .unwrap();
        // Client exposes a trivial App; server imports it.
        fs::write(
            root.join("src/client/main.fai"),
            "# Client app.\ndef App\n    @return Int\ndo\n  1\nend\n",
        )
        .unwrap();
        // Server imports the sibling's App. Also pulls the sibling
        // Client package directly (simulates SSR dependency). The
        // server's lowercase `client` ref should resolve to src/client/,
        // NOT src/server/client/.
        fs::write(
            root.join("src/server/main.fai"),
            "use { App } from client\n\n\
             # Entry.\ndef serverMain\n    @return Int\ndo\n  App()\nend\n",
        )
        .unwrap();
        // Additional top-level source that references the siblings via
        // their capitalised package names. This is what triggers the
        // discovery walker to enter each sibling's directory with
        // package_prefix set.
        let entry_src = "use { App } from client\nuse { serverMain } from Server\n\n\
                        def main\n    @return Void\ndo\nend\n";
        let src_root = root.join("src").to_string_lossy().into_owned();
        let prepared = prepare_source(entry_src, Some(&src_root))
            .unwrap_or_else(|e| panic!("cross-sibling imports should prepare cleanly, got: {}", e));
        let names: Vec<&str> = prepared.modules.iter().map(|m| m.name.as_str()).collect();
        // Whatever the exact resolution order, the bad `Server.client`
        // name should NOT appear — that was the symptom.
        assert!(
            !names.iter().any(|n| *n == "Server.client"),
            "sibling cross-imports should not produce a 'Server.client' \
             module (got: {:?}) — the server's lowercase `client` ref \
             should resolve to the workspace's src/client/",
            names,
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_missing_module_error_hints_at_sibling_subproject() {
        // Scratch project structure mirroring a fullstack layout:
        //   /tmp/<nonce>/fai.toml   contains [project.client] + [project.server]
        //   /tmp/<nonce>/src/server/main.fai   (the "entry" for this test)
        // When load_module_directory fails to find 'client' as a
        // subdirectory of src/server/, the error should suggest the
        // sibling sub-project form rather than the raw OS error.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf = std::env::temp_dir().join(format!("fai-xtarget-{}", nonce));
        let server_src = root.join("src").join("server");
        fs::create_dir_all(&server_src).unwrap();
        fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"XT\"\nversion = \"0.1.0\"\n\n\
             [project.client]\ntarget = \"wasm-html\"\nsource = \"src/client\"\n\n\
             [project.server]\ntarget = \"native\"\nsource = \"src/server\"\n",
        )
        .unwrap();
        let entry = server_src.join("main.fai");
        fs::write(&entry, "def main\n    @return Void\ndo\nend\n").unwrap();

        let err = missing_module_error(
            "client",
            "/nonexistent/path/client",
            Some(&entry),
            "No such file or directory (os error 2)",
        );
        assert!(
            err.contains("sibling sub-project"),
            "error should hint at sibling sub-project, got:\n{}",
            err
        );
        assert!(
            err.contains("[project.client]"),
            "error should reference the matching [project.<name>] section, got:\n{}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_missing_module_error_no_hint_when_name_unknown() {
        // Scratch project WITHOUT any sub-project named 'zzznotreal'.
        // The helper should produce just the bare error with no
        // suggestion — no false positives.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf = std::env::temp_dir().join(format!("fai-xtarget-miss-{}", nonce));
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"Plain\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        let entry = src.join("main.fai");
        fs::write(&entry, "def main\n    @return Void\ndo\nend\n").unwrap();

        let err = missing_module_error(
            "zzznotreal",
            "/nonexistent/zzznotreal",
            Some(&entry),
            "os error 2",
        );
        assert!(
            !err.contains("sibling sub-project"),
            "should not hint about sub-projects when the name doesn't match any: {}",
            err
        );
        assert!(
            err.contains("cannot read module directory"),
            "should keep the original error, got: {}",
            err
        );
        let _ = fs::remove_dir_all(&root);
    }

    // Bytecode-shape assertions (opcode checks, proto counts, register
    // overflow, limit error strings, string-pool contents) lived here
    // through Phase 94.G. Phase H deleted the bytecode emitter; those
    // tests went with it. Equivalent coverage for the direct AST→wasm
    // path is in `fai-codegen-wasm`'s `direct::tests` module (opcode
    // parity, closure emission) and in the CLI wasm_runner tests
    // (end-to-end behavior through the same source fixtures). AST-
    // level discovery/prepare tests are retained below.

    /// Regression: `load_package_deps` needs to walk up to the project
    /// root's `fai.toml`, not just the immediate source directory,
    /// when `source_root = "src"` is configured. AST-level check that
    /// the dep's module is discovered with its function present.
    #[test]
    fn test_load_package_deps_uses_project_root_fai_toml_when_source_root_is_src() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf = std::env::temp_dir().join(format!("fai-compiler-deps-{}", nonce));
        let src = root.join("src");
        let dep_root = root.join("dep");
        let dep_src = dep_root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dep_src).unwrap();

        fs::write(
            root.join("fai.toml"),
            format!(
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\n\"file://{}\" = \"0.1.0\"\n",
                dep_root.display()
            ),
        )
        .unwrap();
        fs::write(
            dep_root.join("fai.toml"),
            "[project]\nname = \"Dep\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        fs::write(
            src.join("main.fai"),
            "use { answer } from Dep\n\ndef main\n    @return Int\ndo\n  answer()\nend\n",
        )
        .unwrap();
        fs::write(
            dep_src.join("dep.fai"),
            "# Answer.\ndef answer\n    @return Int\ndo\n  42\nend\n",
        )
        .unwrap();

        let source = fs::read_to_string(src.join("main.fai")).unwrap();
        let prepared = prepare_source(&source, Some(src.to_str().unwrap())).unwrap();
        let dep = prepared
            .modules
            .iter()
            .find(|m| m.name == "Dep")
            .expect("Dep module loaded");
        let has_answer = dep.statements.iter().any(|s| {
            matches!(
                s, crate::ast::Statement::FunctionDeclaration(fd) if fd.name == "answer"
            )
        });
        assert!(has_answer, "Dep module should expose `answer`");

        let _ = fs::remove_dir_all(root);
    }

    /// Regression: when an external package's module does
    /// `use { answer } from helper`, that `helper` should resolve
    /// against the package's own source root — not the entry
    /// package's. AST-level: `Dep` + `Dep.helper` both appear in
    /// `prepared.modules` with the expected functions.
    #[test]
    fn test_external_package_local_imports_resolve_with_package_namespace() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf =
            std::env::temp_dir().join(format!("fai-compiler-package-local-{}", nonce));
        let src = root.join("src");
        let dep_root = root.join("dep");
        let dep_src = dep_root.join("src");
        let helper_dir = dep_src.join("helper");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&helper_dir).unwrap();

        fs::write(
            root.join("fai.toml"),
            format!(
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\n\"file://{}\" = \"0.1.0\"\n",
                dep_root.display()
            ),
        )
        .unwrap();
        fs::write(
            dep_root.join("fai.toml"),
            "[project]\nname = \"Dep\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        fs::write(
            src.join("main.fai"),
            "use { callAnswer } from Dep\n\ndef main\n    @return Int\ndo\n  callAnswer()\nend\n",
        )
        .unwrap();
        fs::write(dep_src.join("dep.fai"), "use { answer } from helper\n\n# Call answer.\ndef callAnswer\n    @return Int\ndo\n  answer()\nend\n").unwrap();
        fs::write(
            helper_dir.join("helper.fai"),
            "# Answer.\ndef answer\n    @return Int\ndo\n  42\nend\n",
        )
        .unwrap();

        let source = fs::read_to_string(src.join("main.fai")).unwrap();
        let prepared = prepare_source(&source, Some(src.to_str().unwrap())).unwrap();
        let dep = prepared
            .modules
            .iter()
            .find(|m| m.name == "Dep")
            .expect("Dep module");
        assert!(dep.statements.iter().any(|s| matches!(
            s, crate::ast::Statement::FunctionDeclaration(fd) if fd.name == "callAnswer"
        )));
        // The nested `helper` import gets namespaced under `Dep.helper`
        // so it doesn't collide with a local `helper` module the entry
        // package might introduce.
        let dep_helper = prepared
            .modules
            .iter()
            .find(|m| m.name == "Dep.helper")
            .expect("Dep.helper module namespaced under its package");
        assert!(dep_helper.statements.iter().any(|s| matches!(
            s, crate::ast::Statement::FunctionDeclaration(fd) if fd.name == "answer"
        )));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_discover_modules_orders_dependencies_before_importers() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root: PathBuf =
            std::env::temp_dir().join(format!("fai-compiler-module-order-{}", nonce));
        let src = root.join("src");
        let ui_root = root.join("ui");
        let ui_src = ui_root.join("src");
        let html_root = root.join("html");
        let html_src = html_root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&ui_src).unwrap();
        fs::create_dir_all(&html_src).unwrap();

        fs::write(
            root.join("fai.toml"),
            format!(
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\n\"file://{}\" = \"0.1.0\"\n\"file://{}\" = \"0.1.0\"\n",
                ui_root.display(),
                html_root.display(),
            ),
        )
        .unwrap();
        fs::write(
            ui_root.join("fai.toml"),
            "[project]\nname = \"UI\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        fs::write(
            html_root.join("fai.toml"),
            format!(
                "[project]\nname = \"HtmlUI\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\n\"file://{}\" = \"0.1.0\"\n",
                ui_root.display(),
            ),
        )
        .unwrap();
        fs::write(
            src.join("main.fai"),
            "use { inspect } from UI\nuse { htmlRenderer } from HtmlUI\n",
        )
        .unwrap();
        fs::write(
            ui_src.join("ui.fai"),
            "type Renderer\n  selector String\nend\n",
        )
        .unwrap();
        fs::write(
            html_src.join("htmlui.fai"),
            "use { Renderer } from UI\n\n# Create HTML renderer.\ndef htmlRenderer\n    @param selector String\n    @return Renderer\ndo\n  Renderer(selector: selector)\nend\n",
        )
        .unwrap();

        let source = fs::read_to_string(src.join("main.fai")).unwrap();
        let prepared = prepare_source(&source, Some(src.to_str().unwrap())).unwrap();
        let names: Vec<String> = prepared.modules.iter().map(|m| m.name.clone()).collect();
        assert_eq!(names, vec!["UI".to_string(), "HtmlUI".to_string()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_prepare_for_tests_keeps_test_blocks_in_modules() {
        // Regression: in production builds, `load_module_directory`
        // strips `test` blocks from imported module files so `use { X }
        // from mod` doesn't accidentally run `mod`'s tests at init. But
        // the test step needs the opposite — tests in any sibling/nested
        // module file must reach the compiled program so every function
        // in the target tree is exercised.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("fai-compiler-test-mode-{}", nonce));
        let src = root.join("src");
        let sub = src.join("util");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        fs::write(
            src.join("main.fai"),
            "use { shout } from util\n\ndef main\n    @return Void\ndo\n  print(shout('hi'))\nend\n",
        ).unwrap();
        // Module file with both a function AND a test block — the test
        // block should survive `is_test = true` and be stripped under
        // `is_test = false`.
        fs::write(
            sub.join("util.fai"),
            "# Shout a string.\ndef shout\n    @param s String\n    @return String\ndo\n  s\nend\n\ntest shout\nit 'echoes'\n  assert.equals(shout('a'), 'a')\nend\nend\n",
        ).unwrap();

        let source = fs::read_to_string(src.join("main.fai")).unwrap();

        // Production prepare: test block MUST be stripped.
        let prod = prepare_source(&source, Some(src.to_str().unwrap())).unwrap();
        assert!(!prod.is_test);
        let util = prod
            .modules
            .iter()
            .find(|m| m.name == "util")
            .expect("util module loaded");
        let prod_tests = util
            .statements
            .iter()
            .filter(|s| matches!(s, crate::ast::Statement::TestDeclaration(_)))
            .count();
        assert_eq!(
            prod_tests, 0,
            "production build must strip test blocks from imported modules"
        );

        // Test-mode prepare: test block MUST survive.
        let tests = prepare_source_with_synthetic_and_entry_for_tests(
            &source,
            Some(src.to_str().unwrap()),
            Vec::new(),
            Some(src.join("main.fai").to_str().unwrap()),
        )
        .unwrap();
        assert!(tests.is_test);
        let util_t = tests
            .modules
            .iter()
            .find(|m| m.name == "util")
            .expect("util module loaded");
        let test_tests = util_t
            .statements
            .iter()
            .filter(|s| matches!(s, crate::ast::Statement::TestDeclaration(_)))
            .count();
        assert_eq!(
            test_tests, 1,
            "test-mode prepare must keep test blocks in imported modules"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_prepare_for_tests_strips_external_package_test_blocks() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("fai-compiler-test-deps-{}", nonce));
        let app_src = root.join("app/src");
        let pkg_src = root.join("libpkg/src");
        fs::create_dir_all(&app_src).unwrap();
        fs::create_dir_all(&pkg_src).unwrap();
        fs::write(
            root.join("app/fai.toml"),
            format!(
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\n\"file://{}\" = \"0.1.0\"\n",
                root.join("libpkg").display()
            ),
        )
        .unwrap();
        fs::write(
            root.join("libpkg/fai.toml"),
            "[project]\nname = \"LibPkg\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        fs::write(
            app_src.join("main.fai"),
            "use { helper } from LibPkg\n\ndef main\n    @return Int\ndo\n  helper()\nend\n",
        )
        .unwrap();
        fs::write(
            pkg_src.join("libpkg.fai"),
            "# Helper.\ndef helper\n    @return Int\ndo\n  42\nend\n\ntest helper\nit 'works'\n  assert.equals(helper(), 42)\nend\nend\n",
        )
        .unwrap();

        let source = fs::read_to_string(app_src.join("main.fai")).unwrap();
        let tests = prepare_source_with_synthetic_and_entry_for_tests(
            &source,
            Some(app_src.to_str().unwrap()),
            Vec::new(),
            Some(app_src.join("main.fai").to_str().unwrap()),
        )
        .unwrap();
        let pkg = tests
            .modules
            .iter()
            .find(|m| m.name == "LibPkg")
            .expect("external package module loaded");
        let test_count = pkg
            .statements
            .iter()
            .filter(|s| matches!(s, crate::ast::Statement::TestDeclaration(_)))
            .count();
        assert_eq!(
            test_count, 0,
            "app target tests must not run external dependency package tests"
        );

        let _ = fs::remove_dir_all(root);
    }
}

/// Walk statements looking for `use` declarations and load their module files.
/// Recursively discovers imports from loaded module files.
/// `is_test` is propagated down to `load_module_directory` so `test` blocks
/// in imported files are preserved during test runs (they're stripped from
/// production builds so importing a module doesn't accidentally run its
/// tests at module-init time).
fn discover_modules(
    statements: &[fai_parser::ast::Statement],
    source_root: &str,
    synthetic_names: &std::collections::HashSet<String>,
    entry_path: Option<&std::path::Path>,
    entry_ast: Option<&fai_parser::ast::Program>,
    is_test: bool,
) -> Result<Vec<DiscoveredModule>, String> {
    use std::collections::HashSet;

    let mut modules = Vec::new();
    let mut seen = HashSet::new();

    let packages = load_package_deps(source_root);

    for stmt in statements {
        if let fai_parser::ast::Statement::Use(use_stmt) = stmt {
            // Skip synthetic modules — they'll be added separately
            if let Some(first) = use_stmt.module_path.first() {
                if synthetic_names.contains(first) {
                    continue;
                }
            }
            discover_module_use(
                &use_stmt.module_path,
                source_root,
                None,
                &packages,
                &mut seen,
                &mut modules,
                synthetic_names,
                entry_path,
                entry_ast,
                source_root,
                is_test,
            )?;
        }
    }

    Ok(modules)
}

fn discover_module_use(
    path: &[String],
    active_source_root: &str,
    package_prefix: Option<&str>,
    packages: &std::collections::HashMap<String, PackageEntry>,
    seen: &mut std::collections::HashSet<String>,
    modules: &mut Vec<DiscoveredModule>,
    synthetic_names: &std::collections::HashSet<String>,
    entry_path: Option<&std::path::Path>,
    entry_ast: Option<&fai_parser::ast::Program>,
    workspace_source_root: &str,
    is_test: bool,
) -> Result<(), String> {
    if path.first().map(|s| s.as_str()) == Some("std") {
        return Ok(());
    }
    // Skip synthetic modules — they're injected separately
    if let Some(first) = path.first() {
        if synthetic_names.contains(first) {
            return Ok(());
        }
    }

    // If our package_prefix names a SIBLING sub-project, the file we
    // just came from is in workspace src — its lowercase `use`
    // statements should resolve relative to the workspace root, not
    // the sibling's own src. Otherwise a server file doing
    // `use { App } from client` would fruitlessly look for
    // `src/server/client/` (see the `Server.client not found` bug
    // that recursed into a runtime cycle). Uppercase refs keep the
    // current behaviour — they always go through the packages map.
    let parent_is_sibling = package_prefix
        .and_then(|p| packages.get(p))
        .map(|e| e.is_sibling)
        .unwrap_or(false);

    let first_is_upper = path
        .first()
        .map(|s| s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
        .unwrap_or(false);

    let module_name = if let Some(pkg) = package_prefix {
        if first_is_upper || parent_is_sibling {
            // Siblings share the workspace namespace, so we don't
            // prefix their inner lowercase refs either.
            path.join(".")
        } else {
            format!("{}.{}", pkg, path.join("."))
        }
    } else {
        path.join(".")
    };
    if seen.contains(&module_name) {
        return Ok(());
    }
    seen.insert(module_name.clone());

    let first = path.first().map(|s| s.as_str()).unwrap_or("");
    let (module_dir, module_source_root, child_package_prefix, transitive_packages, module_is_test) =
        if first.starts_with(|c: char| c.is_uppercase()) {
            if let Some(pkg_entry) = packages.get(first) {
                let pkg_src_root = &pkg_entry.src_root;
                let dir = if path.len() > 1 {
                    format!("{}/{}", pkg_src_root, path[1..].join("/"))
                } else {
                    pkg_src_root.clone()
                };
                // Load this package's own dependencies so transitive deps are visible
                let mut extended = packages.clone();
                let transitive = load_package_deps(pkg_src_root);
                for (k, v) in transitive {
                    extended.entry(k).or_insert(v);
                }
                let child_pkg_prefix = Some(first.to_string());
                (
                    dir,
                    pkg_src_root.clone(),
                    child_pkg_prefix,
                    extended,
                    is_test && pkg_entry.is_sibling,
                )
            } else {
                return Err(format!(
                    "unknown package '{}' — not found in fai.toml [dependencies]",
                    first
                ));
            }
        } else if parent_is_sibling {
            // Lowercase ref inside a sibling sub-project's source —
            // resolve against the workspace root and forget the
            // sibling's prefix so further transitive walks also see
            // workspace space.
            (
                format!("{}/{}", workspace_source_root, path.join("/")),
                workspace_source_root.to_string(),
                None,
                packages.clone(),
                is_test,
            )
        } else {
            (
                format!("{}/{}", active_source_root, path.join("/")),
                active_source_root.to_string(),
                package_prefix.map(|s| s.to_string()),
                packages.clone(),
                is_test,
            )
        };

    let module = load_module_directory(
        &module_name,
        &module_dir,
        entry_path,
        entry_ast,
        module_is_test,
    )?;

    for file_path in &get_fai_files(&module_dir)? {
        // For the transitive-dependency walk, if this file is the entry
        // file we already parsed its *injected* content — reuse that AST
        // rather than reading the raw on-disk file. Otherwise parse the
        // file fresh just to discover its `use` statements.
        let is_entry = entry_path
            .and_then(|ep| std::fs::canonicalize(file_path).ok().map(|p| p == ep))
            .unwrap_or(false);
        let parsed_ast: Option<fai_parser::ast::Program> = if is_entry {
            None
        } else {
            let source = std::fs::read_to_string(file_path)
                .map_err(|e| format!("cannot read '{}': {}", file_path, e))?;
            fai_parser::parse(&source).ok()
        };
        let file_ast: Option<&fai_parser::ast::Program> = if is_entry {
            entry_ast
        } else {
            parsed_ast.as_ref()
        };
        if let Some(file_ast) = file_ast {
            for stmt in &file_ast.statements {
                if let fai_parser::ast::Statement::Use(use_stmt) = stmt {
                    let mut child_path = use_stmt.module_path.clone();
                    if let Some(pkg) = &child_package_prefix {
                        let is_std = child_path.first().map(|s| s.as_str()) == Some("std");
                        let is_external = child_path
                            .first()
                            .map(|s| s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
                            .unwrap_or(false);
                        // Don't qualify a lowercase ref when the enclosing
                        // package is a SIBLING sub-project — its inner
                        // code shares the workspace namespace, so
                        // `use { App } from client` should resolve to
                        // `src/client/`, not `src/<sibling>/client/`.
                        let enclosing_is_sibling = transitive_packages
                            .get(pkg)
                            .map(|e| e.is_sibling)
                            .unwrap_or(false);
                        if !is_std && !is_external && !enclosing_is_sibling {
                            let mut qualified = vec![pkg.clone()];
                            qualified.extend(child_path);
                            child_path = qualified;
                        }
                    }
                    discover_module_use(
                        &child_path,
                        &module_source_root,
                        child_package_prefix.as_deref(),
                        &transitive_packages,
                        seen,
                        modules,
                        synthetic_names,
                        entry_path,
                        entry_ast,
                        workspace_source_root,
                        is_test,
                    )?;
                }
            }
        }
    }

    modules.push(module);
    Ok(())
}

/// List .fai files in a directory, sorted alphabetically.
fn get_fai_files(dir_path: &str) -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(dir_path)
        .map_err(|e| format!("cannot read directory '{}': {}", dir_path, e))?;
    let mut files: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("fai") {
                Some(p.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    files.sort();
    Ok(files)
}

/// Load package dependencies from fai.toml.
/// Returns a map of package_name → source_root_path.
/// A resolvable package name (uppercase import target) and where its
/// source lives on disk. `is_sibling` distinguishes two flavours:
///
/// - **External package** (`is_sibling = false`) — a real dependency
///   declared under `[dependencies]` with its own `fai.toml`. Lives
///   outside the current workspace. Lowercase `use` statements inside
///   its source resolve relative to *its own* src root.
///
/// - **Sibling sub-project** (`is_sibling = true`) — another
///   `[project.X]` entry in the same workspace fai.toml. Registered
///   so that `use { ... } from Server` works from a client. But
///   unlike a real package, its code shares the workspace namespace:
///   a lowercase `use { App } from client` inside a sibling's source
///   must resolve to the WORKSPACE's `src/client/`, not the
///   sibling's `src/<sibling>/client/`. Previously it didn't and
///   produced confusing `Server.client not found` errors when the
///   server's own code referenced `client`.
#[derive(Debug, Clone)]
pub(crate) struct PackageEntry {
    pub(crate) src_root: String,
    pub(crate) is_sibling: bool,
}

fn load_package_deps(source_root: &str) -> std::collections::HashMap<String, PackageEntry> {
    let mut packages = std::collections::HashMap::new();
    let project_root = resolve_project_root_from_source_root(source_root);
    let toml_path = project_root.join("fai.toml");
    let content = match std::fs::read_to_string(&toml_path) {
        Ok(c) => c,
        Err(_) => return packages,
    };

    // Simple TOML parser: look for [dependencies] section and extract file:// paths
    let mut in_deps = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed == "[dependencies]";
            continue;
        }
        if !in_deps {
            continue;
        }

        // Parse: "file:///path/to/src" = "1.0.0"
        if let Some((key_part, _)) = trimmed.split_once('=') {
            let key = key_part.trim().trim_matches('"');
            if let Some(local_path) = key.strip_prefix("file://") {
                // Read the package's fai.toml to find its name and source_root
                let pkg_toml = format!("{}/fai.toml", local_path);
                if let Ok(pkg_content) = std::fs::read_to_string(&pkg_toml) {
                    let mut pkg_name = String::new();
                    let mut pkg_src = "src".to_string();
                    let mut in_project = false;
                    for pline in pkg_content.lines() {
                        let pt = pline.trim();
                        if pt.starts_with('[') {
                            in_project = pt == "[project]";
                            continue;
                        }
                        if !in_project {
                            continue;
                        }
                        if let Some((k, v)) = pt.split_once('=') {
                            let k = k.trim();
                            let v = v.trim().trim_matches('"');
                            match k {
                                "name" => pkg_name = v.to_string(),
                                "source_root" => pkg_src = v.to_string(),
                                _ => {}
                            }
                        }
                    }
                    if !pkg_name.is_empty() {
                        let full_src = format!("{}/{}", local_path, pkg_src);
                        packages.insert(
                            pkg_name,
                            PackageEntry {
                                src_root: full_src,
                                is_sibling: false,
                            },
                        );
                    }
                }
            }
        }
    }

    // Also resolve sub-project siblings from the workspace fai.toml.
    // Walk up from the source root to find a fai.toml with [project.X]
    // sections and register each sub-project's source as a package.
    let mut search_dir = project_root.clone();
    loop {
        let toml_path = search_dir.join("fai.toml");
        if toml_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&toml_path) {
                let mut section = String::new();
                for line in content.lines() {
                    let t = line.trim();
                    if t.starts_with('[') {
                        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                            section = name.trim().to_string();
                        }
                        continue;
                    }
                    // Look for [project.X] sections with source = "..."
                    if let Some(sub_name) = section.strip_prefix("project.") {
                        let parts: Vec<&str> = sub_name.split('.').collect();
                        if parts.len() == 1 {
                            if let Some((k, v)) = t.split_once('=') {
                                if k.trim() == "source" {
                                    let src_dir = v.trim().trim_matches('"');
                                    let full_src = search_dir.join(src_dir);
                                    if full_src.is_dir() {
                                        let pkg_name = capitalize_first(parts[0]);
                                        if !packages.contains_key(&pkg_name) {
                                            // If source/sub_name/ exists, use that as
                                            // the package root (e.g. src/shared/ for
                                            // sub-project "shared" with source="src").
                                            let sub_dir = full_src.join(parts[0]);
                                            let pkg_src = if sub_dir.is_dir() {
                                                sub_dir
                                            } else {
                                                full_src.clone()
                                            };
                                            packages.insert(
                                                pkg_name,
                                                PackageEntry {
                                                    src_root: pkg_src
                                                        .to_string_lossy()
                                                        .into_owned(),
                                                    is_sibling: true,
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !search_dir.pop() {
            break;
        }
    }

    packages
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Load remote dependencies from fai.toml [remote-dependencies] section.
/// Returns a map of module_name → spec_file_path.
pub fn load_remote_deps(source_root: &str) -> std::collections::HashMap<String, String> {
    let mut deps = std::collections::HashMap::new();
    let project_root = resolve_project_root_from_source_root(source_root);
    let toml_path = project_root.join("fai.toml");
    let content = match std::fs::read_to_string(&toml_path) {
        Ok(c) => c,
        Err(_) => return deps,
    };

    let mut in_remote = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_remote = trimmed == "[remote-dependencies]";
            continue;
        }
        if !in_remote {
            continue;
        }

        // Parse: module_name = { path = "../task_server" }
        // or:    module_name = { url = "https://..." }
        if let Some((key_part, value_part)) = trimmed.split_once('=') {
            let name = key_part.trim().trim_matches('"');
            let value = value_part.trim();

            // Extract path from { path = "..." }
            if let Some(path_start) = value.find("path") {
                if let Some(eq_pos) = value[path_start..].find('=') {
                    let after_eq = &value[path_start + eq_pos + 1..];
                    let path_val = after_eq
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'' || c == ' ' || c == '}');
                    // Resolve relative to the project root
                    let resolved = if path_val.starts_with('/') {
                        path_val.to_string()
                    } else {
                        format!("{}/{}", project_root.display(), path_val)
                    };
                    // Look for interface.json in the resolved path
                    let spec_path = format!("{}/interface.json", resolved);
                    deps.insert(name.to_string(), spec_path);
                }
            }
        }
    }

    deps
}

/// Parse a JSON interface spec and return (module_name, type_exports) for the checker.
pub fn parse_interface_spec(json: &str) -> Result<(String, Vec<(String, String)>), String> {
    // Simple JSON parsing for the spec format
    let parsed: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("Failed to parse interface spec: {}", e))?;

    let name = parsed["name"].as_str().unwrap_or("unknown").to_string();
    let mut exports = Vec::new();

    // Extract function names
    if let Some(functions) = parsed["functions"].as_array() {
        for f in functions {
            if let Some(fname) = f["name"].as_str() {
                exports.push((fname.to_string(), "function".to_string()));
            }
        }
    }

    // Extract type names
    if let Some(types) = parsed["types"].as_array() {
        for t in types {
            if let Some(tname) = t["name"].as_str() {
                exports.push((tname.to_string(), "type".to_string()));
            }
        }
    }

    // Extract enum names
    if let Some(enums) = parsed["enums"].as_array() {
        for e in enums {
            if let Some(ename) = e["name"].as_str() {
                exports.push((ename.to_string(), "enum".to_string()));
            }
        }
    }

    Ok((name, exports))
}

/// Produce a friendly error when a `use { ... } from X` references a
/// module directory that doesn't exist on disk. The default `cannot
/// read module directory 'path'` string is opaque for common fullstack
/// missteps — e.g. a server file importing `{ App } from client` where
/// the compiler's path join lands on `src/server/client` and the real
/// directory is `src/client`. When the missing module name matches a
/// sibling sub-project declared in the workspace's fai.toml, we
/// mention it explicitly.
fn missing_module_error(
    module_name: &str,
    dir_path: &str,
    entry_path: Option<&std::path::Path>,
    os_err: &str,
) -> String {
    // Walk upward from the entry's directory looking for a fai.toml
    // that declares sub-projects. If the missing module name appears
    // there, it's very likely a cross-target import that needs a
    // different path — steer the user toward src/<target>/.
    let mut suggestion = String::new();
    if let Some(entry) = entry_path {
        let mut dir = entry.parent().map(|p| p.to_path_buf());
        while let Some(mut d) = dir {
            let toml_path = d.join("fai.toml");
            if let Ok(content) = std::fs::read_to_string(&toml_path) {
                // Look for `[project.<module_name>]` as a cheap
                // sub-project detector. Avoids pulling the full toml
                // parser into this error path.
                let needle = format!("[project.{}]", module_name);
                if content.contains(&needle) {
                    suggestion = format!(
                        "\n\n       '{}' is a sibling sub-project in fai.toml ([project.{}]).\n       For cross-target imports (e.g. server-side SSR of the client),\n       make sure src/{}/ exists and contains .fai files.\n       The compiler resolves `use {{ X }} from {}` against the source-root sibling,\n       not a subdirectory of the current file.",
                        module_name, module_name, module_name, module_name,
                    );
                    break;
                }
            }
            if !d.pop() {
                break;
            }
            dir = Some(d);
        }
    }
    format!(
        "cannot read module directory '{}': {}{}",
        dir_path, os_err, suggestion
    )
}

/// Load all .fai files from a directory and produce a DiscoveredModule.
fn load_module_directory(
    module_name: &str,
    dir_path: &str,
    entry_path: Option<&std::path::Path>,
    entry_ast: Option<&fai_parser::ast::Program>,
    is_test: bool,
) -> Result<DiscoveredModule, String> {
    let entries = std::fs::read_dir(dir_path)
        .map_err(|e| missing_module_error(module_name, dir_path, entry_path, &e.to_string()))?;

    let mut fai_files: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) == Some("fai") {
                Some(path.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();

    fai_files.sort(); // Alphabetical order, matching TS behavior

    let mut all_statements = Vec::new();
    let mut private_names = Vec::new();
    let mut public_names: Vec<(String, String)> = Vec::new(); // (name, file_path)

    for file_path in &fai_files {
        // When this file is the entry — i.e. the source string the caller
        // already passed to `prepare_source_with_synthetic_and_entry` —
        // reuse the already-parsed `entry_ast` instead of re-reading from
        // disk. The disk copy doesn't contain any injections the CLI
        // applied (peer-hash constants, generated RPC dispatch), so a
        // fresh parse would produce a module that body-fails on names
        // those injections introduced (`addRpcRoutes`, etc.).
        let is_entry = entry_path
            .and_then(|ep| std::fs::canonicalize(file_path).ok().map(|p| p == ep))
            .unwrap_or(false);
        let parsed_ast: Option<fai_parser::ast::Program> = if is_entry {
            None
        } else {
            let source = std::fs::read_to_string(file_path)
                .map_err(|e| format!("cannot read '{}': {}", file_path, e))?;
            Some(fai_parser::parse(&source)?)
        };
        let file_ast: &fai_parser::ast::Program = if is_entry {
            entry_ast.expect("entry_ast must be provided when entry_path matches a module file")
        } else {
            parsed_ast.as_ref().unwrap()
        };

        // Collect private and public declaration names
        for stmt in &file_ast.statements {
            collect_private_names(stmt, &mut private_names);
            collect_public_names(stmt, file_path, &mut public_names);
        }

        // Convert to serde AST statements. Test blocks are normally
        // excluded so importing a module doesn't accidentally run its
        // tests at module-init time in production builds. In test mode
        // (`is_test = true`), we *keep* them so coverage can demand a
        // test for every function across the whole target tree.
        let serde_program = native_bridge::convert_program(file_ast);
        for stmt in serde_program.statements {
            if is_test || !matches!(&stmt, crate::ast::Statement::TestDeclaration(_)) {
                all_statements.push(stmt);
            }
        }
    }

    // Check for duplicate public exports
    let mut seen_names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for (name, file) in &public_names {
        if let Some(prev_file) = seen_names.get(name.as_str()) {
            let prev_basename = std::path::Path::new(prev_file)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let cur_basename = std::path::Path::new(file)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            return Err(format!(
                "duplicate export '{}' in module '{}' (defined in {} and {})",
                name, module_name, prev_basename, cur_basename
            ));
        }
        seen_names.insert(name, file);
    }

    Ok(DiscoveredModule {
        name: module_name.to_string(),
        statements: all_statements,
        private_names,
    })
}

/// Extract names of public (non-private) declarations from a statement.
fn collect_public_names(
    stmt: &fai_parser::ast::Statement,
    file_path: &str,
    out: &mut Vec<(String, String)>,
) {
    match stmt {
        fai_parser::ast::Statement::Function(f) if !f.is_private => {
            out.push((f.name.clone(), file_path.to_string()));
        }
        fai_parser::ast::Statement::Let(l) if !l.is_private => {
            for b in &l.bindings {
                out.push((b.name.clone(), file_path.to_string()));
            }
        }
        fai_parser::ast::Statement::Var(v) if !v.is_private => {
            for b in &v.bindings {
                out.push((b.name.clone(), file_path.to_string()));
            }
        }
        fai_parser::ast::Statement::Type(t) if !t.is_private => {
            out.push((t.name.clone(), file_path.to_string()));
        }
        fai_parser::ast::Statement::Enum(e) if !e.is_private => {
            out.push((e.name.clone(), file_path.to_string()));
        }
        _ => {}
    }
}

/// Extract names of private declarations from a statement.
fn collect_private_names(stmt: &fai_parser::ast::Statement, names: &mut Vec<String>) {
    match stmt {
        fai_parser::ast::Statement::Function(f) if f.is_private => {
            names.push(f.name.clone());
        }
        fai_parser::ast::Statement::Let(l) if l.is_private => {
            for b in &l.bindings {
                names.push(b.name.clone());
            }
        }
        fai_parser::ast::Statement::Var(v) if v.is_private => {
            for b in &v.bindings {
                names.push(b.name.clone());
            }
        }
        fai_parser::ast::Statement::Type(t) if t.is_private => {
            names.push(t.name.clone());
        }
        fai_parser::ast::Statement::Enum(e) if e.is_private => {
            names.push(e.name.clone());
        }
        _ => {}
    }
}
