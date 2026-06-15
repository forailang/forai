//! Async-effect analysis for the direct WASM pipeline.
//!
//! This is the first phase of the real-async plan: identify functions
//! whose bodies may suspend, and propagate that effect through ordinary
//! call sites. The current direct backend is still sync-only, so callers
//! use this metadata to refuse async programs explicitly until async
//! lowering is implemented.

use std::collections::{HashMap, HashSet};

use fai_compiler::ast::{
    AssignmentStatement, AssignmentTarget, BinaryExpression, CallExpression, Expression,
    ForStatement, FunctionDeclaration, IfStatement, SourceLocation, Statement, ThrowStatement,
    TryStatement, VarStatement, WhileStatement,
};
use fai_compiler::compiler::DiscoveredModule;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncCauseKind {
    WaitCall,
    AllCall,
    AsyncCallee(String),
    NowaitBoundary,
    /// Calls `remoteCall(...)` — the RPC client transport, lowered as a
    /// suspending host op so the task yields while the request is in flight.
    RemoteCall,
    /// `let/var x = externCall(...)` for an offloadable (scalar) extern — the
    /// blocking C call is offloaded to the boundary, so the binding is a
    /// suspension point (plan 101 U8). Detected positionally.
    ExternCall,
    /// Invokes a closure *value* (a closure-typed parameter, or a computed
    /// callee like `handlers[i]()` / `cb!()`). Whether that closure suspends
    /// isn't knowable statically, so — staying true to "everything is async" —
    /// the call is a potential suspension point and the invoker is made async,
    /// lowering the call through `Term::AwaitClosure` (which dispatches on the
    /// closure's runtime `frame_size`: sync closure → inline, async → await).
    ClosureCall,
}

#[derive(Debug, Clone)]
pub struct AsyncCause {
    pub kind: AsyncCauseKind,
    pub location: SourceLocation,
    pub file: Option<String>,
    pub module: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct AsyncAnalysis {
    pub async_functions: HashSet<String>,
    pub causes: HashMap<String, AsyncCause>,
    pub scheduler_functions: HashSet<String>,
    pub scheduler_causes: HashMap<String, AsyncCause>,
}

impl AsyncAnalysis {
    pub fn is_empty(&self) -> bool {
        self.async_functions.is_empty() && self.scheduler_functions.is_empty()
    }

    pub fn first_cause(&self) -> Option<(&str, &AsyncCause)> {
        self.async_functions
            .iter()
            .filter_map(|name| self.causes.get(name).map(|cause| (name.as_str(), cause)))
            .chain(self.scheduler_functions.iter().filter_map(|name| {
                self.scheduler_causes
                    .get(name)
                    .map(|cause| (name.as_str(), cause))
            }))
            .min_by_key(|(_, cause)| {
                (
                    cause.file.clone().unwrap_or_default(),
                    cause.location.line,
                    cause.location.column,
                )
            })
    }
}

#[derive(Debug, Clone)]
struct FunctionNode {
    name: String,
    declaration: FunctionDeclaration,
    module: Option<String>,
    file: Option<String>,
    aliases: HashMap<String, String>,
    named_imports: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct DirectCause {
    kind: AsyncCauseKind,
    location: SourceLocation,
}

#[derive(Debug, Clone)]
struct CallSite {
    target: String,
    location: SourceLocation,
}

#[derive(Default)]
struct BodyEffects {
    direct_cause: Option<DirectCause>,
    scheduler_cause: Option<DirectCause>,
    calls: Vec<CallSite>,
}

thread_local! {
    /// Names of offloadable (scalar-signature) extern functions for the program
    /// under analysis — `let x = <one of these>(...)` is a suspension point.
    /// Populated at `analyze` entry; classification is shared with codegen via
    /// `crate::direct::extern_is_offloadable` so the two never diverge.
    static OFFLOADABLE_EXTERN_NAMES: std::cell::RefCell<HashSet<String>> =
        std::cell::RefCell::new(HashSet::new());
}

fn collect_offloadable_extern_names(
    ast: &fai_compiler::ast::Program,
    modules: &[DiscoveredModule],
) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut scan = |stmts: &[Statement]| {
        for s in stmts {
            if let Statement::ExternBlockDeclaration(ext) = s {
                for f in &ext.functions {
                    if crate::direct::extern_is_offloadable(f) {
                        names.insert(f.name.clone());
                    }
                }
            }
        }
    };
    scan(&ast.statements);
    for m in modules {
        scan(&m.statements);
    }
    names
}

/// `Some(location)` if `expr` is a direct call to an offloadable extern.
fn offloadable_extern_call_loc(expr: &Expression) -> Option<SourceLocation> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::IdentifierExpression(id) = &*call.callee else {
        return None;
    };
    if OFFLOADABLE_EXTERN_NAMES.with(|s| s.borrow().contains(&id.name)) {
        Some(call.location.clone())
    } else {
        None
    }
}

pub fn analyze(ast: &fai_compiler::ast::Program, modules: &[DiscoveredModule]) -> AsyncAnalysis {
    OFFLOADABLE_EXTERN_NAMES
        .with(|s| *s.borrow_mut() = collect_offloadable_extern_names(ast, modules));
    let module_function_exports = module_function_exports(modules);
    let mut nodes = Vec::new();
    nodes.extend(collect_functions(
        &ast.statements,
        None,
        &[],
        &module_function_exports,
    ));
    for module in modules {
        nodes.extend(collect_functions(
            &module.statements,
            Some(module.name.as_str()),
            &module.file_paths,
            &module_function_exports,
        ));
    }

    let known_functions: HashSet<String> = nodes.iter().map(|node| node.name.clone()).collect();
    let mut body_effects: HashMap<String, BodyEffects> = HashMap::new();
    for node in &nodes {
        let mut effects = BodyEffects::default();
        for stmt in &node.declaration.body {
            collect_statement_effects(stmt, node, &known_functions, &mut effects);
        }
        body_effects.insert(node.name.clone(), effects);
    }

    let mut out = AsyncAnalysis::default();
    for node in &nodes {
        if let Some(cause) = body_effects
            .get(&node.name)
            .and_then(|effects| effects.direct_cause.clone())
        {
            out.async_functions.insert(node.name.clone());
            out.causes.insert(
                node.name.clone(),
                AsyncCause {
                    kind: cause.kind,
                    location: cause.location,
                    file: node.file.clone(),
                    module: node.module.clone(),
                },
            );
        }
        if let Some(cause) = body_effects
            .get(&node.name)
            .and_then(|effects| effects.scheduler_cause.clone())
        {
            out.scheduler_functions.insert(node.name.clone());
            out.scheduler_causes.insert(
                node.name.clone(),
                AsyncCause {
                    kind: cause.kind,
                    location: cause.location,
                    file: node.file.clone(),
                    module: node.module.clone(),
                },
            );
        }
    }

    loop {
        let mut changed = false;
        for node in &nodes {
            if out.async_functions.contains(&node.name) {
                continue;
            }
            let Some(effects) = body_effects.get(&node.name) else {
                continue;
            };
            // A direct call to a function compiled as a *resume fn* — async
            // (suspends) or scheduler (forks via `nowait`) — forces this caller
            // to be async too: resume fns have a `()->()` signature and can only
            // be invoked through the spawn/await convention, never a direct sync
            // `call`. `nowait`-forked targets are recorded as scheduler causes,
            // not in `effects.calls`, so a function that *only* forks a scheduler
            // callee (the canonical `nowait child()` parent) stays non-async.
            if let Some(call) = effects.calls.iter().find(|call| {
                out.async_functions.contains(&call.target)
                    || out.scheduler_functions.contains(&call.target)
            }) {
                out.async_functions.insert(node.name.clone());
                out.causes.insert(
                    node.name.clone(),
                    AsyncCause {
                        kind: AsyncCauseKind::AsyncCallee(call.target.clone()),
                        location: call.location.clone(),
                        file: node.file.clone(),
                        module: node.module.clone(),
                    },
                );
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    out
}

fn module_function_exports(modules: &[DiscoveredModule]) -> HashMap<String, Vec<String>> {
    let mut exports = HashMap::new();
    for module in modules {
        let mut names = Vec::new();
        for stmt in &module.statements {
            if let Statement::FunctionDeclaration(fd) = stmt {
                if !module.private_names.iter().any(|name| name == &fd.name) {
                    names.push(fd.name.clone());
                }
            }
        }
        exports.insert(module.name.clone(), names);
    }
    exports
}

fn collect_functions(
    statements: &[Statement],
    module: Option<&str>,
    file_paths: &[Option<String>],
    module_function_exports: &HashMap<String, Vec<String>>,
) -> Vec<FunctionNode> {
    let aliases = collect_module_aliases(module, statements);
    let named_imports = collect_named_imports(module, statements, module_function_exports);
    let mut nodes = Vec::new();
    for (idx, stmt) in statements.iter().enumerate() {
        if let Statement::FunctionDeclaration(fd) = stmt {
            let name = match module {
                Some(module) => format!("{}.{}", module, fd.name),
                None => fd.name.clone(),
            };
            nodes.push(FunctionNode {
                name,
                declaration: fd.clone(),
                module: module.map(str::to_string),
                file: file_paths.get(idx).cloned().flatten(),
                aliases: aliases.clone(),
                named_imports: named_imports.clone(),
            });
        }
    }
    nodes
}

fn collect_module_aliases(
    module: Option<&str>,
    statements: &[Statement],
) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    for stmt in statements {
        if let Statement::UseStatement(use_stmt) = stmt {
            if use_stmt.imported_names.is_some() && !use_stmt.import_all {
                continue;
            }
            let canonical = qualify_module_path(module, &use_stmt.module_path);
            if let Some(alias) = use_stmt.module_path.last() {
                aliases.insert(alias.clone(), canonical);
            }
        }
    }
    aliases
}

fn collect_named_imports(
    module: Option<&str>,
    statements: &[Statement],
    module_function_exports: &HashMap<String, Vec<String>>,
) -> HashMap<String, String> {
    let mut imports = HashMap::new();
    for stmt in statements {
        if let Statement::UseStatement(use_stmt) = stmt {
            if use_stmt.import_all {
                let canonical = qualify_module_path(module, &use_stmt.module_path);
                // For `use *`, the sibling guess may not be a real module; fall
                // back to the bare top-level path if that's the one that exists.
                let resolved = if module_function_exports.contains_key(&canonical) {
                    canonical
                } else {
                    let raw = use_stmt.module_path.join(".");
                    if module_function_exports.contains_key(&raw) {
                        raw
                    } else {
                        canonical
                    }
                };
                if let Some(names) = module_function_exports.get(&resolved) {
                    for name in names {
                        imports.insert(name.clone(), format!("{}.{}", resolved, name));
                    }
                }
            } else if let Some(names) = &use_stmt.imported_names {
                for name in names {
                    let resolved =
                        resolve_import_module(module, &use_stmt.module_path, name, module_function_exports);
                    imports.insert(name.clone(), format!("{}.{}", resolved, name));
                }
            }
        }
    }
    imports
}

/// Resolve which module an imported `name` actually comes from. A single-segment
/// import inside a nested module (`use { x } from auth` within `pages.auth`) is
/// ambiguous: it could mean the sibling `pages.auth` or the top-level `auth`.
/// `qualify_module_path` guesses the sibling, but that guess is wrong when the
/// sibling doesn't export the name (or *is* the importing module). Prefer
/// whichever candidate genuinely exports `name`; fall back to the sibling guess.
fn resolve_import_module(
    module: Option<&str>,
    path: &[String],
    name: &str,
    module_function_exports: &HashMap<String, Vec<String>>,
) -> String {
    let exports_name = |m: &str| {
        module_function_exports
            .get(m)
            .is_some_and(|names| names.iter().any(|n| n == name))
    };
    let sibling = qualify_module_path(module, path);
    if exports_name(&sibling) {
        return sibling;
    }
    let raw = path.join(".");
    if exports_name(&raw) {
        return raw;
    }
    sibling
}

fn qualify_module_path(current_module: Option<&str>, path: &[String]) -> String {
    if path.first().map(|s| s.as_str()) == Some("std") {
        return path.join(".");
    }
    let raw = path.join(".");
    if path.len() > 1 {
        return raw;
    }
    if let (Some(current), Some(single)) = (current_module, path.first()) {
        if let Some((parent, _)) = current.rsplit_once('.') {
            return format!("{}.{}", parent, single);
        }
    }
    raw
}

fn collect_statement_effects(
    stmt: &Statement,
    node: &FunctionNode,
    known_functions: &HashSet<String>,
    effects: &mut BodyEffects,
) {
    match stmt {
        Statement::LetStatement(ls) => {
            if let Some(loc) = offloadable_extern_call_loc(&ls.value) {
                set_direct_cause(effects, AsyncCauseKind::ExternCall, loc);
            }
            collect_expression_effects(&ls.value, node, known_functions, effects)
        }
        Statement::VarStatement(VarStatement { value, .. }) => {
            if let Some(loc) = offloadable_extern_call_loc(value) {
                set_direct_cause(effects, AsyncCauseKind::ExternCall, loc);
            }
            collect_expression_effects(value, node, known_functions, effects)
        }
        Statement::AssignmentStatement(AssignmentStatement { target, value, .. }) => {
            match target {
                AssignmentTarget::Field { object } | AssignmentTarget::Index { object } => {
                    collect_expression_effects(object, node, known_functions, effects);
                }
                AssignmentTarget::Variables { .. } => {}
            }
            collect_expression_effects(value, node, known_functions, effects);
        }
        Statement::ExpressionStatement(es) => {
            collect_expression_effects(&es.expression, node, known_functions, effects)
        }
        Statement::IfStatement(IfStatement {
            branches,
            else_branch,
            ..
        }) => {
            for branch in branches {
                collect_expression_effects(&branch.condition, node, known_functions, effects);
                for stmt in &branch.body {
                    collect_statement_effects(stmt, node, known_functions, effects);
                }
            }
            if let Some(else_branch) = else_branch {
                for stmt in else_branch {
                    collect_statement_effects(stmt, node, known_functions, effects);
                }
            }
        }
        Statement::CaseStatement(cs) => {
            collect_expression_effects(&cs.value, node, known_functions, effects);
            for branch in &cs.when_branches {
                collect_expression_effects(&branch.match_expr, node, known_functions, effects);
                for stmt in &branch.body {
                    collect_statement_effects(stmt, node, known_functions, effects);
                }
            }
            if let Some(default_branch) = &cs.default_branch {
                for stmt in default_branch {
                    collect_statement_effects(stmt, node, known_functions, effects);
                }
            }
        }
        Statement::TryStatement(TryStatement {
            try_body,
            catch_body,
            finally_body,
            ..
        }) => {
            for stmt in try_body {
                collect_statement_effects(stmt, node, known_functions, effects);
            }
            for stmt in catch_body {
                collect_statement_effects(stmt, node, known_functions, effects);
            }
            if let Some(finally_body) = finally_body {
                for stmt in finally_body {
                    collect_statement_effects(stmt, node, known_functions, effects);
                }
            }
        }
        Statement::ThrowStatement(ThrowStatement { expression, .. }) => {
            collect_expression_effects(expression, node, known_functions, effects)
        }
        Statement::ForStatement(ForStatement { items, body, .. }) => {
            collect_expression_effects(items, node, known_functions, effects);
            for stmt in body {
                collect_statement_effects(stmt, node, known_functions, effects);
            }
        }
        Statement::WhileStatement(WhileStatement {
            condition, body, ..
        }) => {
            collect_expression_effects(condition, node, known_functions, effects);
            for stmt in body {
                collect_statement_effects(stmt, node, known_functions, effects);
            }
        }
        Statement::ReturnStatement(rs) => {
            if let Some(value) = &rs.value {
                collect_expression_effects(value, node, known_functions, effects);
            }
        }
        Statement::NowaitStatement(nw) => {
            set_scheduler_cause(effects, AsyncCauseKind::NowaitBoundary, nw.location.clone());
        }
        Statement::FunctionDeclaration(_)
        | Statement::FunctionTypeDefDeclaration(_)
        | Statement::UseStatement(_)
        | Statement::TypeDeclaration(_)
        | Statement::EnumDeclaration(_)
        | Statement::TestDeclaration(_)
        | Statement::ExternBlockDeclaration(_)
        | Statement::BreakStatement(_)
        | Statement::ContinueStatement(_) => {}
    }
}

fn collect_expression_effects(
    expr: &Expression,
    node: &FunctionNode,
    known_functions: &HashSet<String>,
    effects: &mut BodyEffects,
) {
    match expr {
        Expression::CallExpression(call) => {
            if let Some((target, kind)) = resolve_call_target(call, node, known_functions) {
                match kind {
                    Some(kind) => set_direct_cause(effects, kind, call.location.clone()),
                    None => effects.calls.push(CallSite {
                        target,
                        location: call.location.clone(),
                    }),
                }
            }
            collect_expression_effects(&call.callee, node, known_functions, effects);
            for arg in &call.args {
                collect_expression_effects(&arg.value, node, known_functions, effects);
            }
        }
        Expression::TemplateStringExpression(ts) => {
            for part in &ts.parts {
                if let fai_compiler::ast::TemplateStringPart::Expression { expression } = part {
                    collect_expression_effects(expression, node, known_functions, effects);
                }
            }
        }
        Expression::ArrayExpression(ae) => {
            for item in &ae.items {
                collect_expression_effects(item, node, known_functions, effects);
            }
        }
        Expression::DictionaryExpression(de) => {
            for entry in &de.entries {
                collect_expression_effects(&entry.value, node, known_functions, effects);
            }
        }
        Expression::TupleExpression(te) => {
            for item in &te.items {
                collect_expression_effects(item, node, known_functions, effects);
            }
        }
        Expression::RangeExpression(re) => {
            collect_expression_effects(&re.start, node, known_functions, effects);
            collect_expression_effects(&re.end, node, known_functions, effects);
        }
        Expression::MemberExpression(me) => {
            collect_expression_effects(&me.object, node, known_functions, effects)
        }
        Expression::UnaryExpression(ue) => {
            collect_expression_effects(&ue.expression, node, known_functions, effects)
        }
        Expression::OptionalCheckExpression(oc) => {
            collect_expression_effects(&oc.expression, node, known_functions, effects)
        }
        Expression::ForceUnwrapExpression(fu) => {
            collect_expression_effects(&fu.expression, node, known_functions, effects)
        }
        Expression::BinaryExpression(BinaryExpression { left, right, .. }) => {
            collect_expression_effects(left, node, known_functions, effects);
            collect_expression_effects(right, node, known_functions, effects);
        }
        Expression::IndexExpression(ie) => {
            collect_expression_effects(&ie.object, node, known_functions, effects);
            collect_expression_effects(&ie.index, node, known_functions, effects);
        }
        Expression::FunctionExpression(fd) => {
            for stmt in &fd.body {
                collect_statement_effects(stmt, node, known_functions, effects);
            }
        }
        Expression::IdentifierExpression(_)
        | Expression::StringExpression(_)
        | Expression::NumberExpression(_)
        | Expression::BooleanExpression(_)
        | Expression::NullExpression(_) => {}
    }
}

fn set_direct_cause(effects: &mut BodyEffects, kind: AsyncCauseKind, location: SourceLocation) {
    if effects.direct_cause.is_none() {
        effects.direct_cause = Some(DirectCause { kind, location });
    }
}

fn set_scheduler_cause(effects: &mut BodyEffects, kind: AsyncCauseKind, location: SourceLocation) {
    if effects.scheduler_cause.is_none() {
        effects.scheduler_cause = Some(DirectCause { kind, location });
    }
}

fn resolve_call_target(
    call: &CallExpression,
    node: &FunctionNode,
    known_functions: &HashSet<String>,
) -> Option<(String, Option<AsyncCauseKind>)> {
    match &*call.callee {
        Expression::IdentifierExpression(id) => {
            let name = id.name.as_str();
            if name == "sleep" {
                return Some((id.name.clone(), Some(AsyncCauseKind::WaitCall)));
            }
            if name == "all" {
                return Some((id.name.clone(), Some(AsyncCauseKind::AllCall)));
            }
            if name == "remoteCall" {
                // The RPC client transport. Lowered as a suspension so the task
                // parks while the request is in flight (browser: async `fetch`),
                // keeping the UI thread free instead of blocking on sync XHR.
                return Some((id.name.clone(), Some(AsyncCauseKind::RemoteCall)));
            }
            if let Some(target) = resolve_bare_function(name, node, known_functions) {
                return Some((target, None));
            }
            // Not a named function/import/builtin. If it's one of this function's
            // own parameters, then it's a closure *value* being invoked — a
            // potential suspension point.
            if node.declaration.params.iter().any(|p| &p.name == name) {
                return Some((id.name.clone(), Some(AsyncCauseKind::ClosureCall)));
            }
            None
        }
        Expression::MemberExpression(me) => {
            let Expression::IdentifierExpression(obj) = &*me.object else {
                return None;
            };
            let canonical = node.aliases.get(&obj.name)?;
            let target = format!("{}.{}", canonical, me.property);
            if known_functions.contains(&target) {
                Some((target, None))
            } else {
                None
            }
        }
        // A computed callee — `handlers[i]()`, `cb!()`, `getCb()()` — can only be
        // a closure value, so invoking it is a potential suspension point.
        Expression::IndexExpression(_)
        | Expression::ForceUnwrapExpression(_)
        | Expression::CallExpression(_) => {
            Some(("<closure>".to_string(), Some(AsyncCauseKind::ClosureCall)))
        }
        _ => None,
    }
}

fn resolve_bare_function(
    name: &str,
    node: &FunctionNode,
    known_functions: &HashSet<String>,
) -> Option<String> {
    if let Some(module) = &node.module {
        let peer = format!("{}.{}", module, name);
        if known_functions.contains(&peer) {
            return Some(peer);
        }
    }
    if known_functions.contains(name) {
        return Some(name.to_string());
    }
    if let Some(imported) = node.named_imports.get(name) {
        if known_functions.contains(imported) {
            return Some(imported.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> fai_compiler::ast::Program {
        fai_compiler::prepare_source(source, None)
            .expect("source should parse")
            .serde_ast
    }

    fn module(name: &str, source: &str) -> DiscoveredModule {
        let ast = parse(source);
        let len = ast.statements.len();
        DiscoveredModule {
            name: name.to_string(),
            statements: ast.statements,
            file_paths: vec![Some(format!("{}.fai", name)); len],
            private_names: Vec::new(),
        }
    }

    #[test]
    fn detects_direct_sleep() {
        let ast = parse("def main\n    @return Int\ndo\n  sleep(10)\n  42\nend\n");
        let analysis = analyze(&ast, &[]);
        assert!(analysis.async_functions.contains("main"));
        assert_eq!(
            analysis.causes.get("main").map(|c| &c.kind),
            Some(&AsyncCauseKind::WaitCall)
        );
    }

    #[test]
    fn propagates_async_callee_to_fixed_point() {
        let ast = parse(
            "def leaf\n    @return Int\ndo\n  sleep(1)\n  1\nend\n\n\
             def mid\n    @return Int\ndo\n  leaf()\nend\n\n\
             def main\n    @return Int\ndo\n  mid()\nend\n",
        );
        let analysis = analyze(&ast, &[]);
        assert!(analysis.async_functions.contains("leaf"));
        assert!(analysis.async_functions.contains("mid"));
        assert!(analysis.async_functions.contains("main"));
        assert_eq!(
            analysis.causes.get("main").map(|c| &c.kind),
            Some(&AsyncCauseKind::AsyncCallee("mid".to_string()))
        );
    }

    #[test]
    fn propagates_across_named_module_import() {
        let entry = parse(
            "use { child } from helper\n\n\
             def main\n    @return Int\ndo\n  child()\nend\n",
        );
        let helper = module(
            "helper",
            "def child\n    @return Int\ndo\n  sleep(1)\n  1\nend\n",
        );
        let analysis = analyze(&entry, &[helper]);
        assert!(analysis.async_functions.contains("helper.child"));
        assert!(analysis.async_functions.contains("main"));
    }

    #[test]
    fn propagates_across_namespace_module_import() {
        let entry = parse(
            "use helper\n\n\
             def main\n    @return Int\ndo\n  helper.child()\nend\n",
        );
        let helper = module(
            "helper",
            "def child\n    @return Int\ndo\n  sleep(1)\n  1\nend\n",
        );
        let analysis = analyze(&entry, &[helper]);
        assert!(analysis.async_functions.contains("helper.child"));
        assert!(analysis.async_functions.contains("main"));
    }

    #[test]
    fn nowait_boundary_requires_scheduler_without_making_parent_async() {
        let ast = parse(
            "def child\n    @return Int\ndo\n  sleep(1)\n  1\nend\n\n\
             def main\n    @return Int\ndo\n  nowait child()\n  0\nend\n",
        );
        let analysis = analyze(&ast, &[]);
        assert!(analysis.async_functions.contains("child"));
        assert!(!analysis.async_functions.contains("main"));
        assert!(analysis.scheduler_functions.contains("main"));
        assert_eq!(
            analysis.scheduler_causes.get("main").map(|c| &c.kind),
            Some(&AsyncCauseKind::NowaitBoundary)
        );
    }
}
