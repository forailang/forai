//! Server-side RPC dispatch generation — generates the dispatch function,
//! HTTP handler, and serve helper from a shared module's remote function
//! declarations.

use fai_parser::ast::{FunctionDeclaration, Statement};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchFunction {
    pub module: Option<String>,
    pub name: String,
    pub key: String,
    pub params: Vec<String>,
    pub returns_void: bool,
    /// `@auth` policy kind (plan 133): "public" | "session". Consumed by
    /// the generated dispatch gate (phase 2). Empty only for legacy
    /// sources — the checker rejects undeclared remote defs.
    pub auth: String,
    /// Named authorizer for `session, <label>: '<name>'` policies.
    pub auth_authorizer: Option<String>,
}

/// Generate server-side RPC dispatch code from a shared module's source.
///
/// Produces:
/// - `__rpcDispatch(fnName, argsJson)` — routes function names to implementations
/// - `__rpcHandler(request)` — HTTP handler that wraps handleRpcRequest
/// - `serve(port)` — starts the RPC server on the given port
pub fn generate_dispatch(shared_source: &str, hash: &str) -> Result<String, String> {
    let program = fai_parser::parse(shared_source)?;

    let mut remote_fns: Vec<DispatchFunction> = Vec::new();
    for stmt in &program.statements {
        if let Statement::Function(fd) = stmt {
            if fd.is_private || fd.name.starts_with('<') || fd.name == "main" {
                continue;
            }
            if fd.is_remote || fd.is_abstract || is_stub_function(fd) {
                remote_fns.push(dispatch_function_from_parser(fd));
            }
        }
    }

    generate_dispatch_for_functions(&remote_fns, hash)
}

/// Generate server-side RPC dispatch code from already discovered remote
/// functions. Module-origin functions are imported by this generated source so
/// a server target can expose RPC endpoints from any reachable app folder.
pub fn generate_dispatch_for_functions(
    remote_fns: &[DispatchFunction],
    hash: &str,
) -> Result<String, String> {
    if remote_fns.is_empty() {
        return Ok(String::new());
    }
    ensure_unique_bare_names(remote_fns)?;

    let mut out = String::new();
    // Imports are generated with the dispatch so server entries do not need
    // to repeat boilerplate for the RPC internals or re-list every reachable
    // endpoint module by hand.
    out.push_str("use std.json\n");
    out.push_str("use std.http.server\n");
    out.push_str("use std.events\n");
    // rpcAuthCheck gates every non-public endpoint before its body runs
    // (plan 133 phase 2); public endpoints get no gate at all.
    let any_gated = remote_fns.iter().any(|fd| fd.auth != "public");
    if any_gated {
        out.push_str("use { handleRpcRequest, rpcAuthCheck, rpcArgsOrNull } from Forui.rpc\n");
    } else {
        out.push_str("use { handleRpcRequest, rpcArgsOrNull } from Forui.rpc\n");
    }
    for (module, names) in import_groups(remote_fns) {
        out.push_str(&format!("use {{ {} }} from {}\n", names.join(", "), module));
    }
    out.push('\n');

    // Generate dispatch function. `ctx` is the per-request context built
    // by Forui.rpc.handleRpcRequest (plan 133): resolved caller identity
    // plus request metadata. The @auth gate (phase 2) consumes it before
    // any handler body runs.
    out.push_str("# Auto-generated RPC dispatch.\ndef __rpcDispatch\n");
    out.push_str("    @param fnName String\n");
    out.push_str("    @param argsJson String\n");
    out.push_str("    @param ctx Dictionary\n");
    out.push_str("    @return String\n");
    out.push_str("do\n");

    for (i, fd) in remote_fns.iter().enumerate() {
        let indent = if i == 0 { "  " } else { &"  ".repeat(i + 1) };
        let else_prefix = if i == 0 { "" } else { "else\n" };
        if i > 0 {
            out.push_str(&format!("{}{}", "  ".repeat(i), else_prefix));
        }
        out.push_str(&format!("{}if {}\n", indent, route_condition(fd)));

        // @auth gate (plan 133 phase 2): runs BEFORE the body — an
        // unauthenticated/unauthorized call never reaches user code.
        // `public` endpoints emit no gate (greppable in generated
        // source); an empty legacy policy fails closed as `session`.
        let gated = fd.auth != "public";
        if gated {
            let policy = if fd.auth.is_empty() {
                "session"
            } else {
                fd.auth.as_str()
            };
            out.push_str(&format!(
                "{}  let __authFail = rpcAuthCheck('{}', '{}', ctx, argsJson)\n",
                indent,
                policy,
                fd.auth_authorizer.as_deref().unwrap_or("")
            ));
            out.push_str(&format!("{}  if __authFail != ''\n", indent));
            out.push_str(&format!("{}    __authFail\n", indent));
            out.push_str(&format!("{}  else\n", indent));
        }

        // Wrap each call in try/catch so errors from RPC functions become
        // JSON error responses rather than WASM traps. Around the call,
        // we fan out three lifecycle events for cross-cutting concerns
        // (logging, metrics, audit): rpc:beforeCall before invocation,
        // rpc:afterCall on success, rpc:error on throw.
        //
        // The catch body produces: {"ok":false,"error":"<message>"}.
        // We build this with string concat to avoid needing escaped quotes in fai.
        let err_prefix = r#"'{"ok":false,"error":"'"#;
        let err_suffix = r#"'"}'"#;
        let before_payload = format!("{{ fnName: '{}' args: argsJson }}", fd.name);
        let after_payload = format!("{{ fnName: '{}' value: __rpcResult }}", fd.name);
        let error_payload = format!("{{ fnName: '{}' message: __e.message }}", fd.name);
        if fd.params.is_empty() {
            out.push_str(&format!("{}  var __rpcResult = ''\n", indent));
            out.push_str(&format!("{}  try\n", indent));
            out.push_str(&format!(
                "{}    events.emit('rpc:beforeCall', {})\n",
                indent, before_payload
            ));
            if fd.returns_void {
                out.push_str(&format!("{}    {}()\n", indent, fd.name));
                out.push_str(&format!("{}    __rpcResult = 'null'\n", indent));
            } else {
                out.push_str(&format!(
                    "{}    __rpcResult = json.stringify({}())\n",
                    indent, fd.name
                ));
            }
            out.push_str(&format!(
                "{}    events.emit('rpc:afterCall', {})\n",
                indent, after_payload
            ));
            out.push_str(&format!("{}  catch __e\n", indent));
            out.push_str(&format!(
                "{}    events.emit('rpc:error', {})\n",
                indent, error_payload
            ));
            out.push_str(&format!(
                "{}    __rpcResult = {} + __e.message + {}\n",
                indent, err_prefix, err_suffix
            ));
            out.push_str(&format!("{}  end\n", indent));
            out.push_str(&format!("{}  __rpcResult\n", indent));
        } else {
            // Arg validation at the boundary (plan 133 phase 3):
            // oversized, malformed, or wrong-arity args answer the fixed
            // 400 envelope BEFORE any parse-derived value reaches the
            // body. 1 MiB is a per-call ceiling on the serialized args,
            // separate from the transport body cap.
            let bad_request = r#"'{"ok":false,"badRequest":true,"error":"bad request"}'"#;
            out.push_str(&format!(
                "{}  if length(argsJson) > 1048576\n",
                indent
            ));
            out.push_str(&format!("{}    {}\n", indent, bad_request));
            out.push_str(&format!("{}  else\n", indent));
            // `rpcArgsOrNull` catches malformed JSON and yields null (json.parse
            // itself now throws), so a bad args payload stays a 400 badRequest
            // here instead of propagating as a 500.
            out.push_str(&format!(
                "{}  let __parsed = rpcArgsOrNull(argsJson)\n",
                indent
            ));
            out.push_str(&format!(
                "{}  if __parsed == null or length(__parsed) != {}\n",
                indent,
                fd.params.len()
            ));
            out.push_str(&format!("{}    {}\n", indent, bad_request));
            out.push_str(&format!("{}  else\n", indent));
            let args: Vec<String> = fd
                .params
                .iter()
                .enumerate()
                .map(|(j, _p)| format!("__parsed[{}]", j))
                .collect();
            let call = format!("{}({})", fd.name, args.join(", "));
            out.push_str(&format!("{}  var __rpcResult = ''\n", indent));
            out.push_str(&format!("{}  try\n", indent));
            out.push_str(&format!(
                "{}    events.emit('rpc:beforeCall', {})\n",
                indent, before_payload
            ));
            if fd.returns_void {
                out.push_str(&format!("{}    {}\n", indent, call));
                out.push_str(&format!("{}    __rpcResult = 'null'\n", indent));
            } else {
                out.push_str(&format!(
                    "{}    __rpcResult = json.stringify({})\n",
                    indent, call
                ));
            }
            out.push_str(&format!(
                "{}    events.emit('rpc:afterCall', {})\n",
                indent, after_payload
            ));
            out.push_str(&format!("{}  catch __e\n", indent));
            out.push_str(&format!(
                "{}    events.emit('rpc:error', {})\n",
                indent, error_payload
            ));
            out.push_str(&format!(
                "{}    __rpcResult = {} + __e.message + {}\n",
                indent, err_prefix, err_suffix
            ));
            out.push_str(&format!("{}  end\n", indent));
            out.push_str(&format!("{}  __rpcResult\n", indent));
            // Close the arity/size validation else branches.
            out.push_str(&format!("{}  end\n", indent));
            out.push_str(&format!("{}  end\n", indent));
        }
        if gated {
            // Close the auth-gate else branch.
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
        .map(|fd| format!("\"{}\"", fd.key))
        .collect();
    // The served spec includes the hash so clients (and tests) can build
    // valid /fai/rpc calls from GET /fai/interface alone.
    let spec = format!(
        "{{\"hash\":\"{}\",\"functions\":[{}]}}",
        hash,
        fn_names.join(",")
    );

    // Generate handler — uses HttpRequest (typed) instead of Dictionary
    out.push_str("# Auto-generated RPC handler.\ndef __rpcHandler\n");
    out.push_str("    @param request HttpRequest\n");
    out.push_str("    @return HttpResponse\n");
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

fn dispatch_function_from_parser(fd: &FunctionDeclaration) -> DispatchFunction {
    DispatchFunction {
        module: None,
        name: fd.name.clone(),
        key: fd.name.clone(),
        params: fd.params.iter().map(|p| p.name.clone()).collect(),
        returns_void: returns_void_parser(fd),
        auth: fd
            .auth_policy
            .as_ref()
            .map(|a| a.kind.clone())
            .unwrap_or_default(),
        auth_authorizer: fd.auth_policy.as_ref().and_then(|a| a.authorizer.clone()),
    }
}

fn returns_void_parser(fd: &FunctionDeclaration) -> bool {
    fd.return_types.len() == 1
        && fd.return_types[0].type_node.name.as_deref() == Some("Void")
        && !fd.return_types[0].type_node.is_array
        && !fd.return_types[0].type_node.is_optional
}

fn ensure_unique_bare_names(remote_fns: &[DispatchFunction]) -> Result<(), String> {
    let mut seen = HashMap::<&str, &str>::new();
    for fd in remote_fns {
        if let Some(prev) = seen.insert(fd.name.as_str(), fd.key.as_str()) {
            if prev != fd.key {
                return Err(format!(
                    "remote function '{}' is exported by both '{}' and '{}'; rename one endpoint before generating addRpcRoutes",
                    fd.name, prev, fd.key
                ));
            }
        }
    }
    Ok(())
}

fn import_groups(remote_fns: &[DispatchFunction]) -> BTreeMap<String, Vec<String>> {
    let mut groups = BTreeMap::<String, Vec<String>>::new();
    for fd in remote_fns {
        if let Some(module) = &fd.module {
            groups
                .entry(module.clone())
                .or_default()
                .push(fd.name.clone());
        }
    }
    for names in groups.values_mut() {
        names.sort();
        names.dedup();
    }
    groups
}

fn route_condition(fd: &DispatchFunction) -> String {
    if fd.key == fd.name {
        format!("fnName == '{}'", fd.name)
    } else {
        format!("fnName == '{}' or fnName == '{}'", fd.key, fd.name)
    }
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
            result.contains("__rpcResult = json.stringify(getTasks())"),
            "no-arg fn stores result"
        );
        assert!(
            result.contains("__rpcResult = json.stringify(addTask(__parsed[0]))"),
            "param fn parses args and stores result. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_generates_rpc_lifecycle_emits() {
        let result = generate_dispatch(
            "remote def getTasks\n    @return Int[]\n\nremote def addTask\n    @param text String\n    @return Int",
            "abc123",
        ).unwrap();
        // std.events import wired up so the emit calls resolve.
        assert!(
            result.contains("use std.events"),
            "dispatcher should import std.events so emit calls resolve. Got:\n{}",
            result
        );
        // beforeCall/afterCall around the no-arg function call.
        assert!(
            result.contains("events.emit('rpc:beforeCall', { fnName: 'getTasks' args: argsJson })"),
            "should emit rpc:beforeCall before getTasks. Got:\n{}",
            result
        );
        assert!(
            result.contains(
                "events.emit('rpc:afterCall', { fnName: 'getTasks' value: __rpcResult })"
            ),
            "should emit rpc:afterCall after getTasks. Got:\n{}",
            result
        );
        // beforeCall/afterCall around the param function call.
        assert!(
            result.contains("events.emit('rpc:beforeCall', { fnName: 'addTask' args: argsJson })"),
            "should emit rpc:beforeCall before addTask. Got:\n{}",
            result
        );
        assert!(
            result
                .contains("events.emit('rpc:afterCall', { fnName: 'addTask' value: __rpcResult })"),
            "should emit rpc:afterCall after addTask. Got:\n{}",
            result
        );
        // error emit fires inside the catch arm of each function.
        assert!(
            result
                .contains("events.emit('rpc:error', { fnName: 'getTasks' message: __e.message })"),
            "should emit rpc:error in getTasks catch. Got:\n{}",
            result
        );
        assert!(
            result.contains("events.emit('rpc:error', { fnName: 'addTask' message: __e.message })"),
            "should emit rpc:error in addTask catch. Got:\n{}",
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
            result.contains("@return HttpResponse"),
            "__rpcHandler should return HttpResponse. Got:\n{}",
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
    fn test_generates_dispatch_for_reachable_module_fns() {
        let result = generate_dispatch_for_functions(
            &[DispatchFunction {
                module: Some("data.tasks".to_string()),
                name: "getTasks".to_string(),
                key: "data.tasks.getTasks".to_string(),
                params: vec![],
                returns_void: false,
                auth: "session".to_string(),
                auth_authorizer: None,
            }],
            "h",
        )
        .unwrap();

        assert!(
            result.contains("use { getTasks } from data.tasks"),
            "should import module function for dispatch. Got:\n{}",
            result
        );
        assert!(
            result.contains("if fnName == 'data.tasks.getTasks' or fnName == 'getTasks'"),
            "should accept module-qualified and legacy route names. Got:\n{}",
            result
        );
        assert!(
            result.contains(r#""functions":["data.tasks.getTasks"]"#),
            "spec should list module-qualified key. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_rejects_ambiguous_bare_remote_names() {
        let err = generate_dispatch_for_functions(
            &[
                DispatchFunction {
                    module: Some("data.tasks".to_string()),
                    name: "get".to_string(),
                    key: "data.tasks.get".to_string(),
                    params: vec![],
                    returns_void: false,
                    auth: "session".to_string(),
                    auth_authorizer: None,
                },
                DispatchFunction {
                    module: Some("auth.tasks".to_string()),
                    name: "get".to_string(),
                    key: "auth.tasks.get".to_string(),
                    params: vec![],
                    returns_void: false,
                    auth: "session".to_string(),
                    auth_authorizer: None,
                },
            ],
            "h",
        )
        .unwrap_err();
        assert!(
            err.contains("remote function 'get' is exported by both"),
            "unexpected error: {}",
            err
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
    fn test_void_remote_def_encodes_null_without_stringifying_void() {
        let result = generate_dispatch(
            "remote def sendMessage\n    @param conversationId Int\n    @param body String\n    @return Void",
            "h",
        )
        .unwrap();
        assert!(
            result.contains("sendMessage(__parsed[0], __parsed[1])"),
            "Void endpoint should be called as a statement. Got:\n{}",
            result
        );
        assert!(
            result.contains("__rpcResult = 'null'"),
            "Void endpoint should encode a null JSON result. Got:\n{}",
            result
        );
        assert!(
            !result.contains("json.stringify(sendMessage("),
            "Void endpoint should not stringify the Void result. Got:\n{}",
            result
        );
    }

    #[test]
    fn test_generated_code_includes_rpc_imports() {
        let result = generate_dispatch("remote def getData\n    @return String", "h").unwrap();
        assert!(
            result.contains("use std.json"),
            "generated dispatch should import std.json. Got:\n{}",
            result
        );
        assert!(
            result.contains("use std.http.server"),
            "generated dispatch should import std.http.server. Got:\n{}",
            result
        );
        assert!(
            result.contains("use { handleRpcRequest, rpcAuthCheck, rpcArgsOrNull } from Forui.rpc"),
            "generated dispatch should import the Forui.rpc handler and the \
             auth gate (the test fns are session-gated). Got:\n{}",
            result
        );
    }
}
