//! Server-side RPC dispatch generation — generates the dispatch function,
//! HTTP handler, and serve helper from a shared module's remote function
//! declarations.

use fai_parser::ast::{FunctionDeclaration, Statement};

/// Generate server-side RPC dispatch code from a shared module's source.
///
/// Produces:
/// - `__rpcDispatch(fnName, argsJson)` — routes function names to implementations
/// - `__rpcHandler(request)` — HTTP handler that wraps handleRpcRequest
/// - `serve(port)` — starts the RPC server on the given port
pub fn generate_dispatch(shared_source: &str, hash: &str) -> Result<String, String> {
    let program = fai_parser::parse(shared_source)?;

    let mut remote_fns: Vec<&FunctionDeclaration> = Vec::new();
    for stmt in &program.statements {
        if let Statement::Function(fd) = stmt {
            if fd.is_private || fd.name.starts_with('<') || fd.name == "main" {
                continue;
            }
            if fd.is_remote || fd.is_abstract || is_stub_function(fd) {
                remote_fns.push(fd);
            }
        }
    }

    if remote_fns.is_empty() {
        return Ok(String::new());
    }

    let mut out = String::new();
    // Note: imports (std.json, std.http.server, Forui.rpc) must be in the
    // server source — we can't add `use` after function definitions.

    // Generate dispatch function
    out.push_str("# Auto-generated RPC dispatch.\ndef __rpcDispatch\n");
    out.push_str("    @param fnName String\n");
    out.push_str("    @param argsJson String\n");
    out.push_str("    @return String\n");
    out.push_str("do\n");

    for (i, fd) in remote_fns.iter().enumerate() {
        let indent = if i == 0 { "  " } else { &"  ".repeat(i + 1) };
        let else_prefix = if i == 0 { "" } else { "else\n" };
        if i > 0 {
            out.push_str(&format!("{}{}", "  ".repeat(i), else_prefix));
        }
        out.push_str(&format!("{}if fnName == '{}'\n", indent, fd.name));

        // Wrap each call in try/catch so errors from RPC functions become
        // JSON error responses rather than WASM traps.
        // The catch body produces: {"ok":false,"error":"<message>"}
        // We build this with string concat to avoid needing escaped quotes in fai.
        let err_prefix = r#"'{"ok":false,"error":"'"#;
        let err_suffix = r#"'"}'"#;
        if fd.params.is_empty() {
            out.push_str(&format!("{}  try\n", indent));
            out.push_str(&format!("{}    json.stringify({}())\n", indent, fd.name));
            out.push_str(&format!("{}  catch __e\n", indent));
            out.push_str(&format!(
                "{}    {} + __e.message + {}\n",
                indent, err_prefix, err_suffix
            ));
            out.push_str(&format!("{}  end\n", indent));
        } else {
            out.push_str(&format!(
                "{}  let __parsed = json.parse(argsJson)\n",
                indent
            ));
            let args: Vec<String> = fd
                .params
                .iter()
                .enumerate()
                .map(|(j, _p)| format!("__parsed[{}]", j))
                .collect();
            let call = format!("{}({})", fd.name, args.join(", "));
            out.push_str(&format!("{}  try\n", indent));
            out.push_str(&format!("{}    json.stringify({})\n", indent, call));
            out.push_str(&format!("{}  catch __e\n", indent));
            out.push_str(&format!(
                "{}    {} + __e.message + {}\n",
                indent, err_prefix, err_suffix
            ));
            out.push_str(&format!("{}  end\n", indent));
        }
    }

    // Close the if/else chain
    let depth = remote_fns.len();
    let close_indent = "  ".repeat(depth);
    out.push_str(&format!("{}else\n{}  ''\n", close_indent, close_indent));
    for i in (0..depth).rev() {
        out.push_str(&format!("{}end\n", "  ".repeat(i + 1)));
    }
    out.push_str("end\n\n");

    // Generate spec JSON
    let fn_names: Vec<String> = remote_fns
        .iter()
        .map(|fd| format!("\"{}\"", fd.name))
        .collect();
    let spec = format!("{{\"functions\":[{}]}}", fn_names.join(","));

    // Generate handler — uses HttpRequest (typed) instead of Dictionary
    out.push_str("# Auto-generated RPC handler.\ndef __rpcHandler\n");
    out.push_str("    @param request HttpRequest\n");
    out.push_str("    @return Dictionary\n");
    out.push_str("do\n");
    out.push_str(&format!("  let __spec = '{}'\n", spec));
    out.push_str(&format!(
        "  handleRpcRequest(request, __spec, '{}', __rpcDispatch)\n",
        hash
    ));
    out.push_str("end\n\n");

    // Generate addRpcRoutes — takes a Router and registers /fai/rpc + /fai/interface
    out.push_str("# Register RPC routes on a router.\ndef addRpcRoutes\n");
    out.push_str("    @param router Router\n");
    out.push_str("    @return Void\n");
    out.push_str("do\n");
    out.push_str("  server.post(router, '/fai/rpc', __rpcHandler)\n");
    out.push_str("  server.get(router, '/fai/interface', __rpcHandler)\n");
    out.push_str("end\n");

    Ok(out)
}

fn is_stub_function(fd: &FunctionDeclaration) -> bool {
    fd.body.len() == 1 && matches!(&fd.body[0], Statement::Throw(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generates_dispatch_for_remote_fns() {
        let result = generate_dispatch(
            "remote def getTasks\n    @return Int[]\n\nremote def addTask\n    @param text String\n    @return Int",
            "abc123",
        ).unwrap();
        assert!(
            result.contains("def __rpcDispatch"),
            "should contain dispatch fn"
        );
        assert!(
            result.contains("if fnName == 'getTasks'"),
            "should route getTasks"
        );
        assert!(
            result.contains("if fnName == 'addTask'"),
            "should route addTask"
        );
        assert!(
            result.contains("json.stringify(getTasks())"),
            "no-arg fn calls directly"
        );
        assert!(
            result.contains("json.stringify(addTask(__parsed[0]))"),
            "param fn parses args. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_generates_handler_with_hash() {
        let result = generate_dispatch("remote def getTasks\n    @return Int[]", "myhash").unwrap();
        assert!(
            result.contains("def __rpcHandler"),
            "should contain handler fn"
        );
        assert!(
            result.contains("@param request HttpRequest"),
            "__rpcHandler should use HttpRequest. Got:\n{}",
            result
        );
        assert!(
            result.contains("handleRpcRequest(request, __spec, 'myhash', __rpcDispatch)"),
            "should pass hash to handleRpcRequest. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_generates_serve_helper() {
        let result = generate_dispatch("remote def getTasks\n    @return Int[]", "h").unwrap();
        assert!(
            result.contains("def addRpcRoutes"),
            "should contain addRpcRoutes fn"
        );
        assert!(
            result.contains("@param router Router"),
            "should take a Router param"
        );
        assert!(
            result.contains("server.post(router, '/fai/rpc', __rpcHandler)"),
            "should register POST /fai/rpc. Got:\n{}",
            result
        );
        assert!(
            result.contains("server.get(router, '/fai/interface', __rpcHandler)"),
            "should register GET /fai/interface. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_generated_add_rpc_routes_parses() {
        let result = generate_dispatch(
            "remote def getTasks\n    @return Int[]\n\nremote def addTask\n    @param text String\n    @return Int",
            "abc",
        ).unwrap();
        let parse_result = fai_parser::parse(&result);
        assert!(
            parse_result.is_ok(),
            "generated addRpcRoutes should be valid forai. Error: {:?}\nSource:\n{}",
            parse_result.err(),
            result
        );
    }

    #[test]
    fn test_generates_spec_json() {
        let result = generate_dispatch(
            "remote def getTasks\n    @return Int[]\n\nremote def addTask\n    @param text String\n    @return Int",
            "h",
        ).unwrap();
        assert!(
            result.contains(r#""functions":["getTasks","addTask"]"#),
            "spec should list function names. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_skips_non_remote_functions() {
        let result = generate_dispatch(
            "def helper\n    @return Int\ndo\n  42\nend\n\nremote def getData\n    @return String",
            "h",
        )
        .unwrap();
        assert!(
            !result.contains("helper"),
            "should skip non-remote functions"
        );
        assert!(result.contains("getData"), "should include remote function");
    }

    #[test]
    fn test_empty_when_no_remote_fns() {
        let result = generate_dispatch("def helper\n    @return Int\ndo\n  42\nend", "h").unwrap();
        assert!(
            result.is_empty(),
            "no remote functions = no dispatch generated"
        );
    }

    #[test]
    fn test_generated_dispatch_parses() {
        let result = generate_dispatch(
            "remote def getTasks\n    @return Int[]\n\nremote def addTask\n    @param text String\n    @return Int\n\nremote def toggle\n    @param id Int\n    @return Bool",
            "abc",
        ).unwrap();
        let parse_result = fai_parser::parse(&result);
        assert!(
            parse_result.is_ok(),
            "generated dispatch + addRpcRoutes should be valid forai. Error: {:?}\nSource:\n{}",
            parse_result.err(),
            result
        );
    }

    #[test]
    fn test_no_use_statements_in_generated_code() {
        // Generated dispatch must NOT include `use` statements — they
        // can't appear after function definitions. The server source
        // is responsible for importing std.json, std.http.server, etc.
        let result = generate_dispatch("remote def getData\n    @return String", "h").unwrap();
        assert!(
            !result.contains("use "),
            "generated dispatch should not contain use statements"
        );
    }
}
