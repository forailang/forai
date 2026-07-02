//! Escape analysis (Phase 2 of memory reclamation — see plans/111).
//!
//! NON-MUTATING. For each function, classifies every local binding whose
//! initializer allocates a fresh heap object as either CONFINED (the object
//! provably does not escape the function — safe to free at scope exit) or
//! ESCAPING (a reference to it may outlive the scope — must not be freed here).
//!
//! Soundness model: forai objects are reference-shared with in-place mutation
//! (see [[forai-memory-model]]), so this is a true escape analysis, not an
//! ownership check. A binding ESCAPES if a reference into it (`x`, `x.f`,
//! `x[i]`, `x!`) reaches a sink — `return`/tail-expression, assignment, a
//! field/index/container store, a closure body, `nowait`, or a call that
//! RETAINS the argument. Conservative = sound: we never under-report an escape.
//!
//! Phase 2.1 (this version): interprocedural param-escape summaries. Instead of
//! treating every non-whitelisted call as retaining its args, we compute — via
//! a call-graph fixpoint — which PARAMETERS of each user function actually
//! escape, so a call retains arg `j` only if the callee's summary says param
//! `j` escapes. Builtins use a non-retaining whitelist; unresolved callees stay
//! conservative (retain all). Closures passed as args are still treated
//! conservatively (a closure referencing a var escapes it regardless of whether
//! the callee retains the closure) — see plans/111 for the closure-precision
//! follow-up, which ties into the single-ownership model.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    AssignmentTarget, Expression, FunctionDeclaration, Statement, TemplateStringPart,
};
use crate::compiler::DiscoveredModule;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitKind {
    /// Produces a fresh heap object this binding owns (literal collection,
    /// closure, or a call that returns a new object).
    Allocates,
    /// Shares an existing object (an alias) or a primitive.
    AliasOrPrimitive,
}

#[derive(Debug, Clone)]
pub struct BindingVerdict {
    pub function: String,
    pub name: String,
    pub init: InitKind,
    pub confined: bool,
}

#[derive(Debug, Default, Clone)]
pub struct EscapeReport {
    pub verdicts: Vec<BindingVerdict>,
}

impl EscapeReport {
    pub fn allocating(&self) -> usize {
        self.verdicts
            .iter()
            .filter(|v| v.init == InitKind::Allocates)
            .count()
    }
    pub fn confined(&self) -> usize {
        self.verdicts
            .iter()
            .filter(|v| v.init == InitKind::Allocates && v.confined)
            .count()
    }
    pub fn summary(&self) -> String {
        let alloc = self.allocating();
        let conf = self.confined();
        let pct = if alloc == 0 {
            0.0
        } else {
            (conf as f64) * 100.0 / (alloc as f64)
        };
        format!(
            "{}/{} allocating bindings provably confined ({:.0}%)",
            conf, alloc, pct
        )
    }
}

// ── Function table + interprocedural context ────────────────────────

struct FnInfo {
    params: Vec<String>,
    body: Vec<Statement>,
    module: Option<String>,
}

/// Per-function set of param indices that escape the function.
type Summaries = HashMap<String, HashSet<usize>>;

struct Ctx<'a> {
    funcs: &'a HashMap<String, FnInfo>,
    summaries: &'a Summaries,
    /// Module of the function currently being analyzed (for resolving bare
    /// identifier calls to peer functions in the same module).
    module: Option<&'a str>,
}

impl<'a> Ctx<'a> {
    /// Resolve a call's callee to a known function key. Returns `(key, ufcs)`
    /// where `ufcs` means the call is `recv.method(args)` and `recv` is the
    /// effective arg 0. Bare `foo(...)` resolves to a peer/entry function;
    /// `recv.method(...)` resolves `method` as a UFCS call. `None` for
    /// builtins, computed callees, and unresolved names.
    fn resolve(&self, callee: &Expression) -> Option<(String, bool)> {
        match callee {
            Expression::IdentifierExpression(id) => self.lookup(&id.name).map(|k| (k, false)),
            Expression::MemberExpression(me) => self.lookup(&me.property).map(|k| (k, true)),
            _ => None,
        }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        if let Some(m) = self.module {
            let q = format!("{m}.{name}");
            if self.funcs.contains_key(&q) {
                return Some(q);
            }
        }
        if self.funcs.contains_key(name) {
            return Some(name.to_string());
        }
        None
    }

    /// Does passing a reference to `var` into `call` retain it (so `var`
    /// escapes via this call)? Considers the UFCS receiver as effective arg 0.
    fn call_retains_var(&self, call: &crate::ast::CallExpression, var: &str) -> bool {
        match self.resolve(&call.callee) {
            Some((key, ufcs)) => {
                let summary = self.summaries.get(&key);
                let mut idx = 0usize;
                // UFCS receiver = effective arg 0.
                if ufcs {
                    if let Expression::MemberExpression(me) = &*call.callee {
                        if aliases_var(&me.object, var)
                            && summary.map_or(false, |s| s.contains(&idx))
                        {
                            return true;
                        }
                    }
                    idx += 1;
                }
                for arg in &call.args {
                    if aliases_var(&arg.value, var) && summary.map_or(false, |s| s.contains(&idx)) {
                        return true;
                    }
                    idx += 1;
                }
                false
            }
            None => {
                // Unknown callee: non-retaining whitelist, else conservative.
                let name = callee_name(&call.callee);
                if name.as_deref().map_or(false, is_non_retaining) {
                    return false;
                }
                let recv_alias = match &*call.callee {
                    Expression::MemberExpression(me) => aliases_var(&me.object, var),
                    _ => false,
                };
                recv_alias || call.args.iter().any(|a| aliases_var(&a.value, var))
            }
        }
    }
}

/// Callees known not to retain their object arguments (pure reads / consumers
/// that return a fresh or primitive value). Used only for callees NOT resolved
/// to a user function. Conservatively small.
pub fn is_non_retaining(callee: &str) -> bool {
    matches!(
        callee,
        "print"
            | "println"
            | "length"
            | "toString"
            | "toInt"
            | "toFloat"
            | "isEmpty"
            | "contains"
            | "startsWith"
            | "endsWith"
            | "indexOf"
            | "trim"
            | "trimStart"
            | "trimEnd"
            | "toUpper"
            | "toLower"
            // String transformers that return a FRESH string/array (no
            // reference into the input — forai strings are immutable values).
            | "replace"
            | "substring"
            | "split"
            | "join"
            | "repeat"
            | "padStart"
            | "padEnd"
            // `copy(x)` reads its arg to build a FRESH duplicate — the arg is
            // not retained (the return is the copy, not the arg).
            | "copy"
            | "abs"
            | "floor"
            | "ceil"
            | "round"
            | "sqrt"
            | "min"
            | "max"
            | "getInt"
            | "getFloat"
            | "getBool"
            | "getString"
            | "has"
            | "keys"
            | "values"
    )
}

fn callee_name(call_callee: &Expression) -> Option<String> {
    match call_callee {
        Expression::IdentifierExpression(id) => Some(id.name.clone()),
        Expression::MemberExpression(me) => Some(me.property.clone()),
        _ => None,
    }
}

/// Does evaluating `expr` yield a REFERENCE into `var`'s object?
fn aliases_var(expr: &Expression, var: &str) -> bool {
    match expr {
        Expression::IdentifierExpression(id) => id.name == var,
        Expression::MemberExpression(me) => aliases_var(&me.object, var),
        Expression::IndexExpression(ie) => aliases_var(&ie.object, var),
        Expression::ForceUnwrapExpression(fu) => aliases_var(&fu.expression, var),
        Expression::OptionalCheckExpression(oc) => aliases_var(&oc.expression, var),
        _ => false,
    }
}

/// Does evaluating `expr` (in any position) cause `var` to escape — a reference
/// into it reaches a retaining sink within `expr`?
fn expr_escapes_var(expr: &Expression, var: &str, ctx: &Ctx) -> bool {
    match expr {
        Expression::CallExpression(call) => {
            if ctx.call_retains_var(call, var) {
                return true;
            }
            // Nested escapes in the callee or args (e.g. `g(h(x))`, or a closure
            // arg that references var — closures stay conservative).
            expr_escapes_var(&call.callee, var, ctx)
                || call
                    .args
                    .iter()
                    .any(|a| expr_escapes_var(&a.value, var, ctx))
        }
        Expression::BinaryExpression(be) => {
            expr_escapes_var(&be.left, var, ctx) || expr_escapes_var(&be.right, var, ctx)
        }
        Expression::UnaryExpression(ue) => expr_escapes_var(&ue.expression, var, ctx),
        Expression::ForceUnwrapExpression(fu) => expr_escapes_var(&fu.expression, var, ctx),
        Expression::OptionalCheckExpression(oc) => expr_escapes_var(&oc.expression, var, ctx),
        Expression::MemberExpression(me) => expr_escapes_var(&me.object, var, ctx),
        Expression::IndexExpression(ie) => {
            expr_escapes_var(&ie.object, var, ctx) || expr_escapes_var(&ie.index, var, ctx)
        }
        // A container literal holding a reference to var: conservatively the
        // container may escape, so var does too.
        Expression::ArrayExpression(ae) => ae
            .items
            .iter()
            .any(|it| aliases_var(it, var) || expr_escapes_var(it, var, ctx)),
        Expression::TupleExpression(te) => te
            .items
            .iter()
            .any(|it| aliases_var(it, var) || expr_escapes_var(it, var, ctx)),
        Expression::DictionaryExpression(de) => de
            .entries
            .iter()
            .any(|e| aliases_var(&e.value, var) || expr_escapes_var(&e.value, var, ctx)),
        // Closure capturing var (conservative: any reference → escape).
        Expression::FunctionExpression(fd) => fn_references_var(fd, var),
        Expression::RangeExpression(re) => {
            expr_escapes_var(&re.start, var, ctx) || expr_escapes_var(&re.end, var, ctx)
        }
        _ => false,
    }
}

fn fn_references_var(fd: &FunctionDeclaration, var: &str) -> bool {
    if fd.params.iter().any(|p| p.name == var) {
        return false;
    }
    fd.body.iter().any(|s| stmt_references_var(s, var))
}

fn stmt_references_var(stmt: &Statement, var: &str) -> bool {
    match stmt {
        Statement::LetStatement(ls) => expr_references_var(&ls.value, var),
        Statement::VarStatement(vs) => expr_references_var(&vs.value, var),
        Statement::AssignmentStatement(a) => {
            let target_ref = match &a.target {
                AssignmentTarget::Field { object } | AssignmentTarget::Index { object } => {
                    expr_references_var(object, var)
                }
                AssignmentTarget::Variables { names } => names.iter().any(|n| n == var),
            };
            target_ref || expr_references_var(&a.value, var)
        }
        Statement::ExpressionStatement(es) => expr_references_var(&es.expression, var),
        Statement::ReturnStatement(rs) => rs
            .value
            .as_ref()
            .map_or(false, |v| expr_references_var(v, var)),
        Statement::ThrowStatement(t) => expr_references_var(&t.expression, var),
        Statement::NowaitStatement(n) => expr_references_var(&n.expression, var),
        Statement::IfStatement(is) => {
            is.branches.iter().any(|b| {
                expr_references_var(&b.condition, var)
                    || b.body.iter().any(|s| stmt_references_var(s, var))
            }) || is
                .else_branch
                .as_ref()
                .map_or(false, |e| e.iter().any(|s| stmt_references_var(s, var)))
        }
        Statement::WhileStatement(ws) => {
            expr_references_var(&ws.condition, var)
                || ws.body.iter().any(|s| stmt_references_var(s, var))
        }
        Statement::ForStatement(fs) => {
            expr_references_var(&fs.items, var)
                || fs.body.iter().any(|s| stmt_references_var(s, var))
        }
        Statement::TryStatement(ts) => {
            ts.try_body.iter().any(|s| stmt_references_var(s, var))
                || ts.catch_body.iter().any(|s| stmt_references_var(s, var))
                || ts
                    .finally_body
                    .as_ref()
                    .map_or(false, |f| f.iter().any(|s| stmt_references_var(s, var)))
        }
        Statement::CaseStatement(cs) => {
            expr_references_var(&cs.value, var)
                || cs.when_branches.iter().any(|b| {
                    expr_references_var(&b.match_expr, var)
                        || b.body.iter().any(|s| stmt_references_var(s, var))
                })
                || cs
                    .default_branch
                    .as_ref()
                    .map_or(false, |d| d.iter().any(|s| stmt_references_var(s, var)))
        }
        _ => false,
    }
}

fn expr_references_var(expr: &Expression, var: &str) -> bool {
    match expr {
        Expression::IdentifierExpression(id) => id.name == var,
        Expression::MemberExpression(me) => expr_references_var(&me.object, var),
        Expression::IndexExpression(ie) => {
            expr_references_var(&ie.object, var) || expr_references_var(&ie.index, var)
        }
        Expression::ForceUnwrapExpression(fu) => expr_references_var(&fu.expression, var),
        Expression::OptionalCheckExpression(oc) => expr_references_var(&oc.expression, var),
        Expression::UnaryExpression(ue) => expr_references_var(&ue.expression, var),
        Expression::BinaryExpression(be) => {
            expr_references_var(&be.left, var) || expr_references_var(&be.right, var)
        }
        Expression::CallExpression(call) => {
            expr_references_var(&call.callee, var)
                || call.args.iter().any(|a| expr_references_var(&a.value, var))
        }
        Expression::ArrayExpression(ae) => ae.items.iter().any(|i| expr_references_var(i, var)),
        Expression::TupleExpression(te) => te.items.iter().any(|i| expr_references_var(i, var)),
        Expression::DictionaryExpression(de) => de
            .entries
            .iter()
            .any(|e| expr_references_var(&e.value, var)),
        Expression::RangeExpression(re) => {
            expr_references_var(&re.start, var) || expr_references_var(&re.end, var)
        }
        Expression::FunctionExpression(fd) => fn_references_var(fd, var),
        _ => false,
    }
}

fn init_kind(expr: &Expression) -> InitKind {
    match expr {
        Expression::ArrayExpression(_)
        | Expression::DictionaryExpression(_)
        | Expression::TupleExpression(_)
        | Expression::FunctionExpression(_) => InitKind::Allocates,
        Expression::CallExpression(_) => InitKind::Allocates, // may return a fresh object
        _ => InitKind::AliasOrPrimitive,
    }
}

/// Does `stmt` cause `var` to escape?
fn stmt_escapes_var(stmt: &Statement, var: &str, ctx: &Ctx) -> bool {
    match stmt {
        Statement::ReturnStatement(rs) => rs.value.as_ref().map_or(false, |v| {
            aliases_var(v, var) || expr_escapes_var(v, var, ctx)
        }),
        Statement::AssignmentStatement(a) => {
            let stored = match &a.target {
                AssignmentTarget::Field { object } | AssignmentTarget::Index { object } => {
                    aliases_var(&a.value, var) || expr_escapes_var(object, var, ctx)
                }
                AssignmentTarget::Variables { .. } => aliases_var(&a.value, var),
            };
            stored || expr_escapes_var(&a.value, var, ctx)
        }
        Statement::LetStatement(ls) => {
            aliases_var(&ls.value, var) || expr_escapes_var(&ls.value, var, ctx)
        }
        Statement::VarStatement(vs) => {
            aliases_var(&vs.value, var) || expr_escapes_var(&vs.value, var, ctx)
        }
        Statement::ExpressionStatement(es) => expr_escapes_var(&es.expression, var, ctx),
        Statement::ThrowStatement(t) => {
            aliases_var(&t.expression, var) || expr_escapes_var(&t.expression, var, ctx)
        }
        // nowait forks a task that outlives the scope → anything it references escapes.
        Statement::NowaitStatement(n) => expr_references_var(&n.expression, var),
        Statement::IfStatement(is) => {
            is.branches.iter().any(|b| {
                expr_escapes_var(&b.condition, var, ctx)
                    || b.body.iter().any(|s| stmt_escapes_var(s, var, ctx))
            }) || is
                .else_branch
                .as_ref()
                .map_or(false, |e| e.iter().any(|s| stmt_escapes_var(s, var, ctx)))
        }
        Statement::WhileStatement(ws) => {
            expr_escapes_var(&ws.condition, var, ctx)
                || ws.body.iter().any(|s| stmt_escapes_var(s, var, ctx))
        }
        Statement::ForStatement(fs) => {
            expr_escapes_var(&fs.items, var, ctx)
                || fs.body.iter().any(|s| stmt_escapes_var(s, var, ctx))
        }
        Statement::TryStatement(ts) => {
            ts.try_body.iter().any(|s| stmt_escapes_var(s, var, ctx))
                || ts.catch_body.iter().any(|s| stmt_escapes_var(s, var, ctx))
                || ts
                    .finally_body
                    .as_ref()
                    .map_or(false, |f| f.iter().any(|s| stmt_escapes_var(s, var, ctx)))
        }
        Statement::CaseStatement(cs) => {
            expr_escapes_var(&cs.value, var, ctx)
                || cs.when_branches.iter().any(|b| {
                    expr_escapes_var(&b.match_expr, var, ctx)
                        || b.body.iter().any(|s| stmt_escapes_var(s, var, ctx))
                })
                || cs
                    .default_branch
                    .as_ref()
                    .map_or(false, |d| d.iter().any(|s| stmt_escapes_var(s, var, ctx)))
        }
        _ => false,
    }
}

/// Tail (implicit return) position aliasing.
fn body_tail_aliases(body: &[Statement], var: &str) -> bool {
    let Some(last) = body.last() else {
        return false;
    };
    match last {
        Statement::ExpressionStatement(es) => aliases_var(&es.expression, var),
        Statement::ReturnStatement(rs) => rs.value.as_ref().map_or(false, |v| aliases_var(v, var)),
        Statement::IfStatement(is) => {
            is.branches.iter().any(|b| body_tail_aliases(&b.body, var))
                || is
                    .else_branch
                    .as_ref()
                    .map_or(false, |e| body_tail_aliases(e, var))
        }
        Statement::CaseStatement(cs) => {
            cs.when_branches
                .iter()
                .any(|b| body_tail_aliases(&b.body, var))
                || cs
                    .default_branch
                    .as_ref()
                    .map_or(false, |d| body_tail_aliases(d, var))
        }
        Statement::TryStatement(ts) => {
            body_tail_aliases(&ts.try_body, var) || body_tail_aliases(&ts.catch_body, var)
        }
        _ => false,
    }
}

/// Names of all `let`/`var` bindings declared anywhere in `body` (recursively).
/// Public so the ownership checker can compute a closure's bound names.
pub fn declared_names(body: &[Statement]) -> Vec<String> {
    let mut names = Vec::new();
    for stmt in body {
        match stmt {
            Statement::LetStatement(ls) => {
                for b in &ls.bindings {
                    names.push(b.name.clone());
                }
            }
            Statement::VarStatement(vs) => {
                for b in &vs.bindings {
                    names.push(b.name.clone());
                }
            }
            Statement::IfStatement(is) => {
                for b in &is.branches {
                    names.extend(declared_names(&b.body));
                }
                if let Some(e) = &is.else_branch {
                    names.extend(declared_names(e));
                }
            }
            Statement::WhileStatement(ws) => names.extend(declared_names(&ws.body)),
            Statement::ForStatement(fs) => names.extend(declared_names(&fs.body)),
            Statement::TryStatement(ts) => {
                names.extend(declared_names(&ts.try_body));
                names.extend(declared_names(&ts.catch_body));
                if let Some(f) = &ts.finally_body {
                    names.extend(declared_names(f));
                }
            }
            Statement::CaseStatement(cs) => {
                for b in &cs.when_branches {
                    names.extend(declared_names(&b.body));
                }
                if let Some(d) = &cs.default_branch {
                    names.extend(declared_names(d));
                }
            }
            _ => {}
        }
    }
    names
}

/// Names (params + locals) of a function that escape, given the current
/// interprocedural summaries.
fn escaping_names(params: &[String], body: &[Statement], ctx: &Ctx) -> HashSet<String> {
    let names: Vec<String> = params.iter().cloned().chain(declared_names(body)).collect();
    let mut esc = HashSet::new();
    for name in &names {
        if body.iter().any(|s| stmt_escapes_var(s, name, ctx)) || body_tail_aliases(body, name) {
            esc.insert(name.clone());
        }
    }
    esc
}

fn collect_binding_verdicts(
    body: &[Statement],
    func: &str,
    escaping: &HashSet<String>,
    out: &mut EscapeReport,
) {
    let push = |name: &str, init: InitKind, out: &mut EscapeReport| {
        out.verdicts.push(BindingVerdict {
            function: func.to_string(),
            name: name.to_string(),
            init,
            confined: init == InitKind::Allocates && !escaping.contains(name),
        });
    };
    for stmt in body {
        match stmt {
            Statement::LetStatement(ls) => {
                let init = init_kind(&ls.value);
                for b in &ls.bindings {
                    push(&b.name, init, out);
                }
            }
            Statement::VarStatement(vs) => {
                let init = init_kind(&vs.value);
                for b in &vs.bindings {
                    push(&b.name, init, out);
                }
            }
            Statement::IfStatement(is) => {
                for b in &is.branches {
                    collect_binding_verdicts(&b.body, func, escaping, out);
                }
                if let Some(e) = &is.else_branch {
                    collect_binding_verdicts(e, func, escaping, out);
                }
            }
            Statement::WhileStatement(ws) => {
                collect_binding_verdicts(&ws.body, func, escaping, out)
            }
            Statement::ForStatement(fs) => collect_binding_verdicts(&fs.body, func, escaping, out),
            Statement::TryStatement(ts) => {
                collect_binding_verdicts(&ts.try_body, func, escaping, out);
                collect_binding_verdicts(&ts.catch_body, func, escaping, out);
                if let Some(f) = &ts.finally_body {
                    collect_binding_verdicts(f, func, escaping, out);
                }
            }
            Statement::CaseStatement(cs) => {
                for b in &cs.when_branches {
                    collect_binding_verdicts(&b.body, func, escaping, out);
                }
                if let Some(d) = &cs.default_branch {
                    collect_binding_verdicts(d, func, escaping, out);
                }
            }
            _ => {}
        }
    }
}

fn collect_functions(
    program: &crate::ast::Program,
    modules: &[DiscoveredModule],
) -> HashMap<String, FnInfo> {
    let mut funcs = HashMap::new();
    for stmt in &program.statements {
        if let Statement::FunctionDeclaration(fd) = stmt {
            funcs.insert(
                fd.name.clone(),
                FnInfo {
                    params: fd.params.iter().map(|p| p.name.clone()).collect(),
                    body: fd.body.clone(),
                    module: None,
                },
            );
        }
    }
    for module in modules {
        for stmt in &module.statements {
            if let Statement::FunctionDeclaration(fd) = stmt {
                funcs.insert(
                    format!("{}.{}", module.name, fd.name),
                    FnInfo {
                        params: fd.params.iter().map(|p| p.name.clone()).collect(),
                        body: fd.body.clone(),
                        module: Some(module.name.clone()),
                    },
                );
            }
        }
    }
    funcs
}

/// Compute per-function param-escape summaries to a fixpoint.
fn compute_summaries(funcs: &HashMap<String, FnInfo>) -> Summaries {
    let mut summaries: Summaries = funcs.keys().map(|k| (k.clone(), HashSet::new())).collect();
    loop {
        let mut changed = false;
        let mut next = summaries.clone();
        for (key, info) in funcs {
            let ctx = Ctx {
                funcs,
                summaries: &summaries,
                module: info.module.as_deref(),
            };
            let escaping = escaping_names(&info.params, &info.body, &ctx);
            let pe: HashSet<usize> = info
                .params
                .iter()
                .enumerate()
                .filter(|(_, p)| escaping.contains(*p))
                .map(|(i, _)| i)
                .collect();
            if next.get(key) != Some(&pe) {
                next.insert(key.clone(), pe);
                changed = true;
            }
        }
        summaries = next;
        if !changed {
            break;
        }
    }
    summaries
}

/// Per-function param-escape summary for the whole program: maps each
/// function's key (entry: bare name; module: `module.name`) to the set of its
/// parameter indices that ESCAPE the function (are retained beyond a call).
/// Used by the ownership move checker to decide call-arg move-vs-borrow: an arg
/// passed to a retaining (escaping) param is MOVED; to a non-retaining param it
/// is BORROWED. UFCS receivers shift params by one — the caller accounts for it.
pub fn param_escape_summaries(
    program: &crate::ast::Program,
    modules: &[DiscoveredModule],
) -> HashMap<String, HashSet<usize>> {
    let funcs = collect_functions(program, modules);
    compute_summaries(&funcs)
}

/// Analyze a whole program: compute interprocedural summaries, then classify
/// every allocating binding as confined or escaping.
pub fn analyze(program: &crate::ast::Program, modules: &[DiscoveredModule]) -> EscapeReport {
    let funcs = collect_functions(program, modules);
    let summaries = compute_summaries(&funcs);
    let mut out = EscapeReport::default();
    for (key, info) in &funcs {
        let ctx = Ctx {
            funcs: &funcs,
            summaries: &summaries,
            module: info.module.as_deref(),
        };
        let escaping = escaping_names(&info.params, &info.body, &ctx);
        collect_binding_verdicts(&info.body, key, &escaping, &mut out);
    }
    out
}

/// An initializer that allocates a fresh, `rt_drop`-sizeable heap object this
/// binding solely owns. Deliberately excludes:
///   - calls (may return a reference INTO a live arg — e.g. `getString`),
///   - string concat/interpolation (calls/binops — fresh, but need a
///     returns-fresh summary to confirm; deferred),
///   - closures (env layout not sized by `rt_drop`).
/// A string LITERAL qualifies: codegen compiles it to `RT_ALLOC_STRING`, which
/// copies the bytes into a fresh heap String (not an interned data-section
/// pointer), and `rt_drop` sizes the STRING tag.
/// So a binding with this init, if confined, is sound to free at scope exit.
pub fn is_freeable_fresh(expr: &Expression) -> bool {
    match expr {
        Expression::ArrayExpression(_)
        | Expression::DictionaryExpression(_)
        | Expression::TupleExpression(_)
        | Expression::StringExpression(_) => true,
        // String interpolation builds a fresh String — EXCEPT the degenerate
        // single-expression form `"{x}"`, which lowers to `value_to_str(x)`
        // with no concat and may return `x` itself (aliasing) when `x` is
        // already a String. Any concat (≥2 parts) or a text part forces a fresh
        // buffer.
        Expression::TemplateStringExpression(ts) => {
            !(ts.parts.len() == 1 && matches!(ts.parts[0], TemplateStringPart::Expression { .. }))
        }
        _ => false,
    }
}

/// Conservative intraprocedural escaping-name set for a function: every call
/// is assumed to retain its args (empty funcs/summaries → unresolved → retain),
/// so this never marks an escaping binding as freeable. Public so the codegen
/// Builder can decide per-binding confinement as it compiles a scope.
pub fn conservative_escaping(fd: &FunctionDeclaration) -> HashSet<String> {
    let funcs: HashMap<String, FnInfo> = HashMap::new();
    let summaries: Summaries = HashMap::new();
    let ctx = Ctx {
        funcs: &funcs,
        summaries: &summaries,
        module: None,
    };
    let params: Vec<String> = fd.params.iter().map(|p| p.name.clone()).collect();
    escaping_names(&params, &fd.body, &ctx)
}

/// Names of single-binding `let`/`var` declarations DIRECTLY in `stmts` whose
/// initializer is a freeable-fresh allocation and that do not escape (per
/// `escaping`). "Directly in" (not nested in an `if`/`case`/inner block) is the
/// keystone for loop bodies: such a binding is set unconditionally on every
/// pass, so freeing it at block exit can't double-free a stale local.
///
/// Single-binding only: `let a, b = expr` destructures `expr`'s elements into
/// `a`/`b`, so neither name owns the temporary.
pub fn confined_freeable_names(stmts: &[Statement], escaping: &HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for stmt in stmts {
        let (bindings, value) = match stmt {
            Statement::LetStatement(ls) => (&ls.bindings, &ls.value),
            Statement::VarStatement(vs) => (&vs.bindings, &vs.value),
            _ => continue,
        };
        if bindings.len() != 1 || !is_freeable_fresh(value) {
            continue;
        }
        let name = &bindings[0].name;
        if !escaping.contains(name) {
            out.push(name.clone());
        }
    }
    out
}

/// Plan function-scope-exit drops: top-level confined fresh-literal bindings,
/// safe to `rt_drop` before the function returns.
pub fn plan_drops(fd: &FunctionDeclaration) -> Vec<String> {
    let escaping = conservative_escaping(fd);
    confined_freeable_names(&fd.body, &escaping)
}

/// Does any statement in `stmts` (recursively) use the `return` keyword?
fn body_has_return(stmts: &[Statement]) -> bool {
    stmts.iter().any(|s| match s {
        Statement::ReturnStatement(_) => true,
        Statement::IfStatement(is) => {
            is.branches.iter().any(|b| body_has_return(&b.body))
                || is
                    .else_branch
                    .as_ref()
                    .map_or(false, |e| body_has_return(e))
        }
        Statement::WhileStatement(ws) => body_has_return(&ws.body),
        Statement::ForStatement(fs) => body_has_return(&fs.body),
        Statement::TryStatement(ts) => {
            body_has_return(&ts.try_body)
                || body_has_return(&ts.catch_body)
                || ts
                    .finally_body
                    .as_ref()
                    .map_or(false, |f| body_has_return(f))
        }
        Statement::CaseStatement(cs) => {
            cs.when_branches.iter().any(|b| body_has_return(&b.body))
                || cs
                    .default_branch
                    .as_ref()
                    .map_or(false, |d| body_has_return(d))
        }
        _ => false,
    })
}

/// Function-scope drops safe to emit at ANY completion terminator of a
/// resumable (async) lowering: the top-level confined fresh-literal bindings,
/// but ONLY when the function has no early `return`. Async frames are reused
/// (freelist), so a binding slot not yet written on the path to a completion
/// holds stale garbage; dropping it would free a live object. With no early
/// `return` every completion is the tail, by which point every top-level
/// (unconditional) binding has been assigned — so the drop is safe.
pub fn plan_async_completion_drops(fd: &FunctionDeclaration) -> Vec<String> {
    if body_has_return(&fd.body) {
        return Vec::new();
    }
    plan_drops(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_src(src: &str) -> EscapeReport {
        let prepared = crate::prepare_source(src, None).expect("prepare");
        analyze(&prepared.serde_ast, &prepared.modules)
    }

    fn verdict<'a>(r: &'a EscapeReport, name: &str) -> &'a BindingVerdict {
        r.verdicts
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("no binding {name}; have {:?}", r.verdicts))
    }

    #[test]
    fn confined_local_temporary() {
        let r = analyze_src(
            "def main\n    @return Void\ndo\n  let s = ['a' 'b']\n  print(length(s))\nend\n",
        );
        assert!(verdict(&r, "s").confined, "{:?}", verdict(&r, "s"));
    }

    #[test]
    fn returned_value_escapes() {
        let r = analyze_src("def make\n    @return Int[]\ndo\n  let xs = [1 2 3]\n  xs\nend\n");
        assert!(!verdict(&r, "xs").confined, "{:?}", verdict(&r, "xs"));
    }

    #[test]
    fn stored_into_container_escapes() {
        let r = analyze_src(concat!(
            "def main\n    @return Void\ndo\n",
            "  var acc Dictionary = {}\n",
            "  let row = [1 2]\n",
            "  acc = set(acc, 'r', row)\n",
            "  print(length(acc))\n",
            "end\n",
        ));
        assert!(!verdict(&r, "row").confined, "{:?}", verdict(&r, "row"));
    }

    #[test]
    fn interprocedural_non_retaining_user_call_keeps_confined() {
        // `consume` only reads its param (prints it) → doesn't retain it. So
        // `xs`, passed to `consume`, stays CONFINED — the interprocedural
        // summary recovers what the conservative v1 lost.
        let r = analyze_src(concat!(
            "# reads but does not retain.\n",
            "def consume\n    @param ys Int[]\n    @return Void\ndo\n  print(length(ys))\nend\n",
            "\n",
            "def main\n    @return Void\ndo\n  let xs = [1 2 3]\n  consume(xs)\nend\n",
        ));
        assert!(verdict(&r, "xs").confined, "{:?}", verdict(&r, "xs"));
    }

    fn drops_of(src: &str, fn_name: &str) -> Vec<String> {
        let prepared = crate::prepare_source(src, None).expect("prepare");
        let fd = prepared
            .serde_ast
            .statements
            .iter()
            .find_map(|s| match s {
                Statement::FunctionDeclaration(fd) if fd.name == fn_name => Some(fd),
                _ => None,
            })
            .expect("function");
        plan_drops(fd)
    }

    #[test]
    fn plan_drops_picks_confined_literal_skips_escaping_and_calls() {
        // `a` is a confined dict literal and `d` a confined string literal, both
        // used only by non-retaining reads → freeable. `b` is returned →
        // escapes. `c`'s init is a call (may alias) → not a freeable-fresh
        // literal.
        let names = drops_of(
            concat!(
                "def f\n    @return Int[]\ndo\n",
                "  let a = {}\n",
                "  let c = keys(a)\n",
                "  let d = 'hi'\n",
                "  let b = [1 2 3]\n",
                "  print(length(d))\n",
                "  b\n",
                "end\n",
            ),
            "f",
        );
        assert_eq!(
            names,
            vec!["a".to_string(), "d".to_string()],
            "confined dict + string literals; call-result and returned excluded"
        );
    }

    #[test]
    fn plan_async_completion_drops_disabled_by_early_return() {
        let src = concat!(
            "def f\n    @param cond Bool\n    @return Int\ndo\n",
            "  let a = [1 2 3]\n",
            "  if cond\n    return 0\n  end\n",
            "  length(a)\nend\n",
        );
        // `a` is confined, but the early `return` means a completion can be
        // reached before `a` is set on some path → not safe for async drops.
        assert!(
            drops_of(src, "f").contains(&"a".to_string()),
            "sync plan still lists it"
        );
        let prepared = crate::prepare_source(src, None).expect("prepare");
        let fd = prepared
            .serde_ast
            .statements
            .iter()
            .find_map(|s| match s {
                Statement::FunctionDeclaration(fd) if fd.name == "f" => Some(fd),
                _ => None,
            })
            .unwrap();
        assert!(
            plan_async_completion_drops(fd).is_empty(),
            "async plan suppressed by the early return"
        );
    }

    #[test]
    fn interprocedural_retaining_user_call_escapes() {
        // `keep` stores its param into a module global → it retains it → `xs`
        // escapes through the call.
        let r = analyze_src(concat!(
            "var saved Int[] = []\n",
            "# retains: stores the param into a global.\n",
            "def keep\n    @param ys Int[]\n    @return Void\ndo\n  saved = ys\nend\n",
            "\n",
            "def main\n    @return Void\ndo\n  let xs = [1 2 3]\n  keep(xs)\nend\n",
        ));
        assert!(!verdict(&r, "xs").confined, "{:?}", verdict(&r, "xs"));
    }
}
