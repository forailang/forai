//! RPC proxy generation — generates client-side wrapper functions for
//! remote dependencies. Given a shared module's source code (containing
//! abstract function declarations), produces forai source code that wraps
//! each function with `remoteCall`.

use fai_parser::ast::{FunctionDeclaration, Statement};

/// Generate RPC proxy source code from a shared module's source.
///
/// For each abstract (body-less) public function in `shared_source`,
/// generates a concrete function that serializes args and calls
/// `remoteCall(url, fnName, argsJson, hash)`.
///
/// Returns generated forai source code that can be prepended to the
/// client's source before compilation.
pub fn generate_proxies(shared_source: &str, url: &str, hash: &str) -> Result<String, String> {
    let program = fai_parser::parse(shared_source)?;

    let mut output = String::new();

    // Emit type declarations for remote types first (clients need them to type-check responses).
    for stmt in &program.statements {
        if let Statement::Type(td) = stmt {
            if td.is_remote && !td.is_private {
                output.push_str(&format!("type {}\n", td.name));
                for field in &td.fields {
                    let mut line =
                        format!("  {} {}", field.name, format_type_node(&field.type_node));
                    for attr in &field.attributes {
                        match &attr.value {
                            fai_parser::ast::FieldAttributeValue::String(s) => {
                                line.push_str(&format!(", {}: '{}'", attr.key, s));
                            }
                            fai_parser::ast::FieldAttributeValue::Flag => {
                                line.push_str(&format!(", {}", attr.key));
                            }
                        }
                    }
                    output.push_str(&line);
                    output.push('\n');
                }
                output.push_str("end\n\n");
            }
        }
    }

    output.push_str("use std.json\n\n");

    for stmt in &program.statements {
        if let Statement::Function(fd) = stmt {
            if fd.is_private || fd.name.starts_with('<') || fd.name == "main" {
                continue;
            }
            // Generate proxy for remote functions, abstract functions,
            // or old-style stubs with throw bodies (backwards compat)
            if fd.is_remote || fd.is_abstract || is_stub_function(fd) {
                output.push_str(&generate_proxy_fn(fd, url, hash));
                output.push('\n');
            }
        }
    }

    Ok(output)
}

/// Check if a function is a stub (body is just `throw Error(...)`)
fn is_stub_function(fd: &FunctionDeclaration) -> bool {
    // A stub has exactly one statement which is a throw
    fd.body.len() == 1 && matches!(&fd.body[0], Statement::Throw(_))
}

/// Generate a single proxy function.
fn generate_proxy_fn(fd: &FunctionDeclaration, url: &str, hash: &str) -> String {
    let mut out = String::new();

    // Doc comment — prefix each line with #
    if let Some(doc) = &fd.doc_comment {
        for line in doc.lines() {
            out.push_str(&format!("# {}\n", line));
        }
    }

    // Function signature
    out.push_str(&format!("def {}\n", fd.name));
    for param in &fd.params {
        let type_str = format_type_node(&param.type_node);
        out.push_str(&format!("    @param {} {}\n", param.name, type_str));
    }
    for ret in &fd.return_types {
        let type_str = format_type_node(&ret.type_node);
        out.push_str(&format!("    @return {}\n", type_str));
    }

    // Body: serialize args and call remoteCall
    out.push_str("do\n");
    if fd.params.is_empty() {
        out.push_str(&format!(
            "  remoteCall('{}', '{}', '[]', '{}')\n",
            url, fd.name, hash
        ));
    } else {
        // Build JSON array of args: '[' + json.stringify(a) + ',' + json.stringify(b) + ']'
        let parts: Vec<String> = fd
            .params
            .iter()
            .map(|p| format!("json.stringify({})", p.name))
            .collect();
        out.push_str(&format!(
            "  let __args = '[' + {} + ']'\n",
            parts.join(" + ',' + ")
        ));
        out.push_str(&format!(
            "  remoteCall('{}', '{}', __args, '{}')\n",
            url, fd.name, hash
        ));
    }
    out.push_str("end\n");

    out
}

/// Format a TypeNode back to source. Minimal — handles the common cases.
fn format_type_node(tn: &fai_parser::ast::TypeNode) -> String {
    let mut s = tn.name.clone().unwrap_or_else(|| "Void".to_string());
    if tn.is_array {
        s.push_str("[]");
    }
    if tn.is_optional {
        s.push('?');
    }
    s
}

/// Generate RPC proxy source code from a server's schema JSON.
/// Produces both type declarations AND proxy functions.
/// This is the new path — reads the server's schema instead of shared source.
pub fn generate_proxies_from_schema(schema_json: &str, url: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(schema_json).map_err(|e| format!("invalid schema JSON: {}", e))?;

    let hash = parsed["hash"].as_str().unwrap_or("");
    let mut output = String::new();

    // Generate type declarations
    if let Some(types) = parsed["types"].as_array() {
        for t in types {
            let name = t["name"].as_str().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            output.push_str(&format!("type {}\n", name));
            if let Some(fields) = t["fields"].as_array() {
                for f in fields {
                    let fname = f["name"].as_str().unwrap_or("");
                    let ftype = f["type"].as_str().unwrap_or("String");
                    let mut line = format!("  {} {}", fname, ftype);
                    if let Some(alias) = f["alias"].as_str() {
                        line.push_str(&format!(", alias: \"{}\"", alias));
                    }
                    if f["omit"].as_bool().unwrap_or(false) {
                        line.push_str(", omit");
                    }
                    output.push_str(&line);
                    output.push('\n');
                }
            }
            output.push_str("end\n\n");
        }
    }

    // Generate proxy functions
    output.push_str("use std.json\n\n");
    if let Some(functions) = parsed["functions"].as_array() {
        for f in functions {
            let name = f["name"].as_str().unwrap_or("");
            if name.is_empty() {
                continue;
            }

            // Doc comment
            output.push_str(&format!("# Auto-generated RPC proxy for {}.\n", name));
            output.push_str(&format!("def {}\n", name));

            // Params
            let params = f["params"].as_array();
            if let Some(params) = params {
                for p in params {
                    let pname = p["name"].as_str().unwrap_or("");
                    let ptype = p["type"].as_str().unwrap_or("String");
                    output.push_str(&format!("    @param {} {}\n", pname, ptype));
                }
            }

            // Return type
            if let Some(returns) = f["returns"].as_array() {
                if let Some(ret) = returns.first() {
                    output.push_str(&format!("    @return {}\n", ret.as_str().unwrap_or("Void")));
                }
            }

            // Body
            output.push_str("do\n");
            let param_names: Vec<&str> = params
                .map(|ps| ps.iter().filter_map(|p| p["name"].as_str()).collect())
                .unwrap_or_default();

            if param_names.is_empty() {
                output.push_str(&format!(
                    "  remoteCall('{}', '{}', '[]', '{}')\n",
                    url, name, hash
                ));
            } else {
                let parts: Vec<String> = param_names
                    .iter()
                    .map(|p| format!("json.stringify({})", p))
                    .collect();
                output.push_str(&format!(
                    "  let __args = '[' + {} + ']'\n",
                    parts.join(" + ',' + ")
                ));
                output.push_str(&format!(
                    "  remoteCall('{}', '{}', __args, '{}')\n",
                    url, name, hash
                ));
            }
            output.push_str("end\n\n");
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a schema JSON with N remote types and N remote fns.
    /// Used to probe the register-overflow boundary for the generated
    /// Server module — the partners project (8 types + 9 defs) hit the
    /// compiler's u8 register limit and stalled the agent session.
    fn synthetic_schema(n: usize) -> String {
        let mut s = String::from("{\"name\":\"\",\"version\":\"\",\"hash\":\"abc\",\"types\":[");
        for i in 1..=n {
            if i > 1 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"name\":\"T{i}\",\"fields\":[{{\"name\":\"a\",\"type\":\"Int\"}},{{\"name\":\"b\",\"type\":\"String\"}},{{\"name\":\"c\",\"type\":\"Bool\"}}]}}",
            ));
        }
        s.push_str("],\"functions\":[");
        for i in 1..=n {
            if i > 1 {
                s.push(',');
            }
            s.push_str(&format!(
                "{{\"name\":\"fn{i}\",\"params\":[{{\"name\":\"x\",\"type\":\"Int\",\"hasDefault\":false}},{{\"name\":\"y\",\"type\":\"String\",\"hasDefault\":false}}],\"returns\":[\"T{i}\"]}}",
            ));
        }
        s.push_str("]}");
        s
    }

    #[test]
    fn test_rpc_proxy_compiles_at_moderate_scale() {
        // Regression: a moderate-size partners RPC proxy (20 remote types +
        // 20 remote defs) should parse and type-check cleanly. The prior
        // shape of this test asserted a bytecode register count — that
        // concern is moot under the direct AST→wasm path (Phase H),
        // but we keep a scaled fixture to guard against parser/checker
        // regressions at this shape.
        let schema = synthetic_schema(20);
        let proxy_src = generate_proxies_from_schema(&schema, "http://x").unwrap();
        let prepared = fai_compiler::prepare_source(&proxy_src, None)
            .expect("20-type RPC proxy should prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("20-type RPC proxy should type-check");
    }

    #[test]
    fn test_generate_proxy_no_params() {
        let result = generate_proxies(
            "# Get tasks.\ndef getTasks\n    @return Task[]",
            "http://localhost:3040",
            "abc123",
        )
        .unwrap();
        assert!(
            result.contains("def getTasks"),
            "should contain function def"
        );
        assert!(
            result.contains("@return Task[]"),
            "should preserve return type"
        );
        assert!(
            result.contains("remoteCall('http://localhost:3040', 'getTasks', '[]', 'abc123')"),
            "should call remoteCall with correct args. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_generate_proxy_with_params() {
        let result = generate_proxies(
            "# Add a task.\ndef addTask\n    @param text String\n    @return Task",
            "http://localhost:3040",
            "hash1",
        )
        .unwrap();
        assert!(
            result.contains("def addTask"),
            "should contain function def"
        );
        assert!(
            result.contains("@param text String"),
            "should preserve param"
        );
        assert!(
            result.contains("json.stringify(text)"),
            "should serialize param"
        );
        assert!(
            result.contains("remoteCall('http://localhost:3040', 'addTask', __args, 'hash1')"),
            "should call remoteCall. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_generate_proxy_multiple_params() {
        let result = generate_proxies(
            "def update\n    @param id Int\n    @param text String\n    @return Bool",
            "http://api.example.com",
            "xyz",
        )
        .unwrap();
        assert!(
            result.contains("json.stringify(id) + ',' + json.stringify(text)"),
            "should join multiple params with comma. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_skips_main_and_private() {
        let result = generate_proxies(
            "def main\n    @return Void\ndo\n  print('hi')\nend\n\n# Get.\ndef getTasks\n    @return Int[]",
            "http://localhost:3040",
            "h",
        ).unwrap();
        assert!(!result.contains("def main"), "should skip main");
        assert!(
            result.contains("def getTasks"),
            "should include abstract function"
        );
    }

    #[test]
    fn test_skips_concrete_non_stub_functions() {
        let result = generate_proxies(
            "def helper\n    @return Int\ndo\n  42\nend\n\n# Get.\ndef getData\n    @return String",
            "http://localhost:3040",
            "h",
        )
        .unwrap();
        assert!(
            !result.contains("def helper"),
            "should skip concrete functions"
        );
        assert!(
            result.contains("def getData"),
            "should include abstract function"
        );
    }

    #[test]
    fn test_handles_stub_functions() {
        // Old-style stubs with `throw Error(...)` should also generate proxies
        let result = generate_proxies(
            "# Get tasks.\ndef getTasks\n    @return Int[]\ndo\n  throw Error('stub')\nend",
            "http://localhost:3040",
            "h",
        )
        .unwrap();
        assert!(
            result.contains("def getTasks"),
            "should generate proxy for stub function"
        );
        assert!(result.contains("remoteCall"), "should contain remoteCall");
    }

    #[test]
    fn test_generate_proxy_for_remote_def() {
        let result = generate_proxies(
            "# Get.\nremote def getTasks\n    @return Int[]\n\n# Not remote.\ndef helper\n    @return Int\ndo\n  42\nend",
            "http://localhost:3040",
            "h",
        ).unwrap();
        assert!(
            result.contains("def getTasks"),
            "should generate proxy for remote def"
        );
        assert!(
            !result.contains("def helper"),
            "should skip non-remote concrete functions"
        );
    }

    #[test]
    fn test_skips_non_remote_abstract() {
        // An abstract function WITHOUT remote should still get a proxy
        // (backwards compat for Phase 2 abstract defs)
        let result = generate_proxies(
            "def getData\n    @return String",
            "http://localhost:3040",
            "h",
        )
        .unwrap();
        assert!(
            result.contains("def getData"),
            "abstract functions should still get proxies"
        );
    }

    // ── generate_proxies_from_schema tests ──────────────────────

    #[test]
    fn test_schema_generates_types() {
        let schema = r#"{"hash":"h1","functions":[],"types":[{"name":"Task","fields":[{"name":"id","type":"Int"},{"name":"text","type":"String"}]}],"enums":[]}"#;
        let result = generate_proxies_from_schema(schema, "http://localhost:3040").unwrap();
        assert!(
            result.contains("type Task"),
            "should generate type. Got:\n{}",
            result
        );
        assert!(result.contains("id Int"), "should have id field");
        assert!(result.contains("text String"), "should have text field");
        assert!(result.contains("end"), "should close type");
    }

    #[test]
    fn test_schema_generates_proxies() {
        let schema = r#"{"hash":"abc","functions":[{"name":"getTasks","params":[],"returns":["Task[]"],"type_params":[]}],"types":[],"enums":[]}"#;
        let result = generate_proxies_from_schema(schema, "http://localhost:3040").unwrap();
        assert!(result.contains("def getTasks"), "should generate function");
        assert!(
            result.contains("remoteCall('http://localhost:3040', 'getTasks', '[]', 'abc')"),
            "should call remoteCall. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_schema_generates_both_types_and_proxies() {
        let schema = r#"{"hash":"x","functions":[{"name":"addTask","params":[{"name":"text","type":"String"}],"returns":["Task"],"type_params":[]}],"types":[{"name":"Task","fields":[{"name":"id","type":"Int"}]}],"enums":[]}"#;
        let result = generate_proxies_from_schema(schema, "http://localhost:3040").unwrap();
        assert!(result.contains("type Task"), "should have type");
        assert!(result.contains("def addTask"), "should have function");
        assert!(
            result.contains("json.stringify(text)"),
            "should serialize params"
        );
    }

    #[test]
    fn test_includes_json_import() {
        let result = generate_proxies(
            "def getData\n    @return String",
            "http://localhost:3040",
            "h",
        )
        .unwrap();
        assert!(
            result.contains("use std.json"),
            "should include json import"
        );
    }

    #[test]
    fn test_inject_rpc_proxies_integration() {
        // Set up a mini project directory:
        //   project_root/
        //     fai.toml
        //     shared/src/shared.fai  (abstract functions)
        //     client/src/client.fai  (client code)
        use std::fs;
        let dir = std::env::temp_dir().join(format!("fai_rpc_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("shared/src")).unwrap();
        fs::create_dir_all(dir.join("client/src")).unwrap();

        // fai.toml with remote dep config
        fs::write(
            dir.join("fai.toml"),
            concat!(
            "[project]\nname = \"TestApp\"\nversion = \"0.1.0\"\n\n",
            "[project.shared]\nsource = \"shared/src\"\n\n",
            "[project.client]\ntarget = \"wasm-html\"\nsource = \"client/src\"\n\n",
            "[project.client.dependencies.shared.remote.dev]\nurl = \"http://localhost:9999\"\n",
        ),
        )
        .unwrap();

        // Shared module with abstract functions
        fs::write(
            dir.join("shared/src/shared.fai"),
            concat!(
                "type Item\n  id Int\n  name String\nend\n\n",
                "# Get items.\ndef getItems\n    @return Item[]\n\n",
                "# Add item.\ndef addItem\n    @param name String\n    @return Item\n",
            ),
        )
        .unwrap();

        // Generate proxy modules from the project root
        let source_root_str = dir.join("client/src").to_string_lossy().to_string();
        let modules = crate::generate_rpc_proxy_modules(Some(&source_root_str));

        // Verify proxies were generated
        assert!(
            !modules.is_empty(),
            "should generate at least one proxy module"
        );
        let all_source: String = modules
            .iter()
            .map(|(_, s)| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all_source.contains("def getItems"),
            "should contain getItems proxy. Source:\n{}",
            all_source
        );
        assert!(
            all_source.contains("def addItem"),
            "should contain addItem proxy. Source:\n{}",
            all_source
        );
        assert!(
            all_source.contains("remoteCall('http://localhost:9999'"),
            "should contain remoteCall with correct URL. Source:\n{}",
            all_source
        );
        assert!(
            all_source.contains("json.stringify(name)"),
            "should serialize params. Source:\n{}",
            all_source
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generated_code_parses() {
        // The generated proxy source should be valid forai
        let result = generate_proxies(
            "def getTasks\n    @return Int[]\n\n# Add.\ndef addItem\n    @param text String\n    @return Int",
            "http://localhost:3040",
            "abc",
        ).unwrap();
        // Try parsing the generated code — it should be valid syntax
        let parse_result = fai_parser::parse(&result);
        assert!(
            parse_result.is_ok(),
            "generated proxy should be valid forai. Got error: {:?}\nSource:\n{}",
            parse_result.err(),
            result
        );
    }

    // ── field attribute preservation ─────────────────────────────────

    #[test]
    fn test_generate_proxies_preserves_alias_in_type() {
        // remote type with alias attribute — generated type decl should include it
        let result = generate_proxies(
            "remote type User\n  userName String, alias: 'user_name'\nend\n\n# Get user.\nremote def getUser\n    @return User",
            "http://localhost:3040",
            "h",
        ).unwrap();
        assert!(
            result.contains("alias: 'user_name'"),
            "generated type should preserve alias attribute. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_generate_proxies_preserves_omit_in_type() {
        let result = generate_proxies(
            "remote type Payload\n  data String\n  secret String, omit\nend\n\n# Get payload.\nremote def getData\n    @return Payload",
            "http://localhost:3040",
            "h",
        ).unwrap();
        assert!(
            result.contains(", omit"),
            "generated type should preserve omit attribute. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_generate_proxies_with_attributes_parses() {
        // Generated source with attributes should be valid forai
        let result = generate_proxies(
            "remote type User\n  userName String, alias: 'user_name'\n  password String, omit\nend\n\n# Get.\nremote def getUser\n    @return User",
            "http://localhost:3040",
            "h",
        ).unwrap();
        let parse_result = fai_parser::parse(&result);
        assert!(
            parse_result.is_ok(),
            "generated proxy with attributes should be valid forai. Error: {:?}\nSource:\n{}",
            parse_result.err(),
            result
        );
    }

    #[test]
    fn test_schema_preserves_alias_in_type() {
        let schema = r#"{"hash":"h","functions":[],"types":[{"name":"User","fields":[{"name":"userName","type":"String","hasDefault":false,"alias":"user_name"}]}],"enums":[]}"#;
        let result = generate_proxies_from_schema(schema, "http://localhost:3040").unwrap();
        assert!(
            result.contains("alias: \"user_name\""),
            "generated type from schema should include alias. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_schema_preserves_omit_in_type() {
        let schema = r#"{"hash":"h","functions":[],"types":[{"name":"Payload","fields":[{"name":"secret","type":"String","hasDefault":false,"omit":true}]}],"enums":[]}"#;
        let result = generate_proxies_from_schema(schema, "http://localhost:3040").unwrap();
        assert!(
            result.contains(", omit"),
            "generated type from schema should include omit. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_schema_type_without_attributes_has_no_extras() {
        let schema = r#"{"hash":"h","functions":[],"types":[{"name":"Task","fields":[{"name":"id","type":"Int","hasDefault":false}]}],"enums":[]}"#;
        let result = generate_proxies_from_schema(schema, "http://localhost:3040").unwrap();
        assert!(
            !result.contains("alias"),
            "plain field should have no alias. Got:\n{}",
            result
        );
        assert!(
            !result.contains("omit"),
            "plain field should have no omit. Got:\n{}",
            result
        );
    }
}
