use super::*;

// ── Real async engine: resume-body lowering + module assembly (R2) ──
//
// Compiles an async `main` into a *resume function* (`() -> ()`) over the
// guest scheduler in `async_engine`. v1 handles the narrow straight-line
// shape — statements that don't suspend plus `sleep(<number>)` suspension
// points, no locals across a suspension — which is enough for the
// `sleep_ordering` acceptance fixture. Returns `None` (fall back to the
// facade / sync path) for anything outside that shape.

/// `sleep(expr)` as a statement → the millisecond expression, else `None`.
fn async_sleep_arg_of(stmt: &Statement) -> Option<&Expression> {
    let Statement::ExpressionStatement(es) = stmt else {
        return None;
    };
    let Expression::CallExpression(call) = &es.expression else {
        return None;
    };
    let Expression::IdentifierExpression(callee) = &*call.callee else {
        return None;
    };
    if callee.name != "sleep" {
        return None;
    }
    let [arg] = call.args.as_slice() else {
        return None;
    };
    Some(&arg.value)
}

/// If `expr` is `remoteCall(url, fn, args, hash)` (the RPC client transport),
/// return its 4 argument expressions and call-site location. Lowered as a
/// suspending host op (`Term::AwaitRemote`) so the task yields while the
/// request is in flight.
fn remote_call_args(
    expr: &Expression,
) -> Option<(Vec<&Expression>, &fai_compiler::ast::SourceLocation)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::IdentifierExpression(id) = &*call.callee else {
        return None;
    };
    if id.name != "remoteCall" || call.args.len() != 4 {
        return None;
    }
    Some((call.args.iter().map(|a| &a.value).collect(), &call.location))
}

fn host_op_call_args<'a>(
    expr: &'a Expression,
    fns: &AsyncResolve<'_>,
) -> Option<(
    i32,
    Vec<&'a Expression>,
    &'a fai_compiler::ast::SourceLocation,
)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let (op_kind, arity) = match &*call.callee {
        Expression::MemberExpression(me) if !fns.is_ufcs_call(call) => {
            let Expression::IdentifierExpression(obj) = &*me.object else {
                return None;
            };
            let canonical = fns.aliases.get(&obj.name)?;
            stdlib_host_op_kind(canonical, &me.property)?
        }
        Expression::IdentifierExpression(id) => {
            let imported = fns.named_imports.get(&id.name)?;
            let (module, method) = imported.rsplit_once('.')?;
            stdlib_host_op_kind(module, method)?
        }
        _ => return None,
    };
    if !arity.accepts(call.args.len()) {
        return None;
    }
    Some((
        op_kind,
        call.args.iter().map(|a| &a.value).collect(),
        &call.location,
    ))
}

/// If `expr` is a direct call to a *user* function (one of `fns`), return
/// its name and argument expressions. Builtins (`print`, `sleep`, `all`,
/// `Error`, …) are not user functions and never match. In the "everything
/// is async" model, every user-function call is an auto-await.
/// Module-aware name resolution for the async lowering. Mirrors
/// `async_analysis::resolve_bare_function` / `resolve_call_target` so the
/// qualified names produced here match the analysis' async set (and the
/// `{module}.{fn}`-prefixed function table). For single-file programs the
/// maps are empty and `module_context` is `None`, so resolution is identity.
pub(super) struct AsyncResolve<'a> {
    /// Qualified names of async (task) functions — awaits/spawns target these.
    pub(super) async_set: &'a std::collections::HashSet<String>,
    /// Qualified names of every user function (for "is this a known fn?").
    pub(super) all_fns: &'a std::collections::HashSet<String>,
    /// Namespace aliases: `obj` in `obj.fn` → canonical module path.
    pub(super) aliases: &'a std::collections::HashMap<String, String>,
    /// Named imports: bare `f` → `{module}.f`.
    pub(super) named_imports: &'a std::collections::HashMap<String, String>,
    /// The module a function being lowered belongs to (peer-call resolution).
    pub(super) module_context: Option<&'a str>,
    /// Call sites the checker rewrote via UFCS (`recv.method()` → `method(recv)`),
    /// keyed by `(module_key, line, col)` — same key `compile_call` uses.
    pub(super) ufcs_calls: &'a std::collections::HashSet<(String, u32, u32)>,
    /// This function's `module_key` (file path, else module context) — the
    /// first element of a UFCS call-site key.
    pub(super) module_key: &'a str,
}

#[derive(Clone, Copy)]
struct AsyncCallArg<'a> {
    label: Option<&'a str>,
    value: &'a Expression,
}

impl<'a> AsyncResolve<'a> {
    /// Whether this call site was rewritten via UFCS by the checker.
    fn is_ufcs_call(&self, call: &CallExpression) -> bool {
        self.ufcs_calls.contains(&(
            self.module_key.to_string(),
            call.location.line,
            call.location.column,
        ))
    }

    /// Resolve a bare identifier to its canonical user-fn name.
    fn resolve_bare(&self, name: &str) -> Option<String> {
        if let Some(m) = self.module_context {
            let peer = format!("{}.{}", m, name);
            if self.all_fns.contains(&peer) {
                return Some(peer);
            }
        }
        if self.all_fns.contains(name) {
            return Some(name.to_string());
        }
        if let Some(imported) = self.named_imports.get(name) {
            if self.all_fns.contains(imported) {
                return Some(imported.clone());
            }
        }
        None
    }

    /// Resolve a member call `obj.prop` to its canonical name via aliases.
    fn resolve_member(&self, obj: &str, prop: &str) -> Option<String> {
        let canonical = self.aliases.get(obj)?;
        let target = format!("{}.{}", canonical, prop);
        if self.all_fns.contains(&target) {
            Some(target)
        } else {
            None
        }
    }

    /// Resolve any call expression's callee to a canonical user-fn name.
    fn resolve_call(&self, call: &CallExpression) -> Option<String> {
        match &*call.callee {
            Expression::IdentifierExpression(id) => self.resolve_bare(&id.name),
            Expression::MemberExpression(me) => {
                // UFCS (`recv.method(...)` → `method(recv, ...)`): the checker
                // recorded this site, so `method` resolves as a free function.
                if self.is_ufcs_call(call) {
                    return self.resolve_bare(&me.property);
                }
                // Otherwise a namespace-member call (`alias.fn`).
                let Expression::IdentifierExpression(obj) = &*me.object else {
                    return None;
                };
                self.resolve_member(&obj.name, &me.property)
            }
            _ => None,
        }
    }

    fn is_async(&self, name: &str) -> bool {
        self.async_set.contains(name)
    }
}

fn async_call_args<'a>(call: &'a CallExpression, fns: &AsyncResolve<'_>) -> Vec<AsyncCallArg<'a>> {
    let mut args: Vec<AsyncCallArg<'a>> = Vec::with_capacity(call.args.len() + 1);
    if fns.is_ufcs_call(call) {
        if let Expression::MemberExpression(me) = &*call.callee {
            args.push(AsyncCallArg {
                label: None,
                value: &me.object,
            });
        }
    }
    args.extend(call.args.iter().map(|a| AsyncCallArg {
        label: a.label.as_deref(),
        value: &a.value,
    }));
    args
}

/// If `expr` is a call to an *async* user function, return its canonical name
/// and arg expressions. Sync user calls and builtins return `None` (they flow
/// through `compile_call` as plain direct calls).
fn user_callee<'a>(
    expr: &'a Expression,
    fns: &AsyncResolve<'_>,
) -> Option<(
    String,
    Vec<AsyncCallArg<'a>>,
    &'a fai_compiler::ast::SourceLocation,
)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let resolved = fns.resolve_call(call)?;
    if !fns.is_async(&resolved) {
        return None;
    }
    Some((resolved, async_call_args(call, fns), &call.location))
}

/// Async-closure compilation context, threaded into `BuildContext` so a
/// closure encountered mid-body can be detected — and, in later A3.0 steps,
/// lowered as a resume fn (frame leads with `env_ptr`, params follow). Present
/// only on the real-engine path; `None` on the pure-sync builder.
#[derive(Clone, Copy)]
pub(crate) struct AsyncClosureCtx<'a> {
    pub(super) async_set: &'a std::collections::HashSet<String>,
    pub(super) all_fns: &'a std::collections::HashSet<String>,
    pub(super) layout: &'a crate::async_engine::SchedLayout,
    pub(super) fn_table_idx: &'a std::collections::HashMap<String, u32>,
    pub(super) frame_sizes: &'a std::collections::HashMap<String, i32>,
}

/// A closure whose body awaits or forks must be compiled as a resume fn
/// (A3.0) — detected by the same suspension check used for named functions.
pub(super) fn closure_is_async(fd: &FunctionDeclaration, r: &AsyncResolve<'_>) -> bool {
    stmts_have_suspension(&fd.body, r)
}

/// If `expr` is a call whose callee is an async closure *literal*
/// (`(do…end)(args)`), return the closure expression + its args. The literal's
/// async-ness is statically known, so the call is a suspension point with no
/// runtime sync/async dispatch (that's the closure-typed-*value* case, later).
fn async_closure_call<'a>(
    expr: &'a Expression,
    r: &AsyncResolve<'_>,
) -> Option<(&'a Expression, Vec<&'a Expression>)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::FunctionExpression(fd) = &*call.callee else {
        return None;
    };
    if !closure_is_async(fd, r) {
        return None;
    }
    Some((&*call.callee, call.args.iter().map(|a| &a.value).collect()))
}

/// If `expr` is a call whose callee is a closure-typed *parameter* (`p(args)`),
/// return the callee expression + args. Such a call may suspend (the closure
/// could be async), so it's lowered as an await with runtime sync/async
/// dispatch — the checker guarantees a called param is function-typed.
/// A call whose callee is a closure *value* rather than a named function:
/// invoking a closure-typed parameter (`children()`), or a computed callee
/// (`handlers[i]()`, `cb!()`, `getCb()()`). These dispatch through the closure
/// header and — mirroring `async_analysis`'s `ClosureCall` cause — are lowered
/// as `Term::AwaitClosure` so a suspending closure parks the caller instead of
/// being driven by a re-entrant `poll`. Must match the analysis's detection or
/// a function flagged async there could hit a CFG shape it can't lower here.
fn indirect_closure_call<'a>(
    expr: &'a Expression,
    params: &std::collections::HashSet<String>,
    fns: &AsyncResolve<'_>,
) -> Option<(&'a Expression, Vec<&'a Expression>)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let is_closure_callee = match &*call.callee {
        Expression::IdentifierExpression(id) => params.contains(&id.name),
        Expression::IndexExpression(_)
        | Expression::ForceUnwrapExpression(_)
        | Expression::CallExpression(_) => true,
        // A member call is a closure-valued *field* invocation
        // (`matched!.builder()`, `state.onUpdate()`) only when it's NEITHER a
        // UFCS rewrite (`recv.method()` → `method(recv)`) NOR a namespace-member
        // call on a module alias (`array.append`, `json.stringify`). Those two
        // resolve to named functions / builtins and are lowered as ordinary
        // calls — routing them through AwaitClosure (treating a builtin as a
        // closure value) corrupts the scheduler. This mirrors `compile_call`'s
        // routing, which only falls through to the closure path here.
        Expression::MemberExpression(me) => {
            // `assert` is magically in scope inside test blocks with no `use`
            // statement, so the alias map never carries it — but its calls
            // are namespace dispatches, not closure fields (plan 103 U6).
            let obj_is_module_alias = matches!(
                &*me.object,
                Expression::IdentifierExpression(id)
                    if fns.aliases.contains_key(&id.name) || id.name == "assert"
            );
            !fns.is_ufcs_call(call) && !obj_is_module_alias
        }
        _ => false,
    };
    if !is_closure_callee {
        return None;
    }
    Some((&*call.callee, call.args.iter().map(|a| &a.value).collect()))
}

/// Whether any statement (recursively) references a user-function call.
/// Used to gate `try` bodies: error propagation out of an awaited child is
/// not implemented yet, so a `try` containing an await falls back.
fn stmts_have_user_call(stmts: &[Statement], fns: &AsyncResolve<'_>) -> bool {
    stmts.iter().any(|s| stmt_has_user_call(s, fns))
}

/// Whether any statement (recursively) suspends — a `sleep` or a user call.
/// Used to gate a value-`try`'s `finally`: the try-result is held in a wasm
/// local that wouldn't survive a suspension inside `finally`.
fn stmts_have_suspension(stmts: &[Statement], fns: &AsyncResolve<'_>) -> bool {
    stmts
        .iter()
        .any(|s| async_sleep_arg_of(s).is_some() || stmt_has_user_call(s, fns))
}

/// Whether `stmts` contain a `break`/`continue` that targets the *enclosing*
/// loop — i.e. not buried inside a nested `for`/`while` (those bind to the inner
/// loop). Used to decide whether a `for` loop can be safely desugared into an
/// index `while` (a `continue` would skip the manual index increment).
fn stmts_have_loop_control(stmts: &[Statement]) -> bool {
    stmts.iter().any(|s| match s {
        Statement::BreakStatement(_) | Statement::ContinueStatement(_) => true,
        Statement::IfStatement(is) => {
            is.branches.iter().any(|b| stmts_have_loop_control(&b.body))
                || is
                    .else_branch
                    .as_ref()
                    .is_some_and(|e| stmts_have_loop_control(e))
        }
        Statement::TryStatement(ts) => {
            stmts_have_loop_control(&ts.try_body)
                || stmts_have_loop_control(&ts.catch_body)
                || ts
                    .finally_body
                    .as_ref()
                    .is_some_and(|f| stmts_have_loop_control(f))
        }
        // A nested for/while owns its own break/continue; don't descend.
        _ => false,
    })
}

fn stmts_have_return(stmts: &[Statement]) -> bool {
    stmts.iter().any(stmt_has_return)
}

fn stmt_has_return(stmt: &Statement) -> bool {
    match stmt {
        Statement::ReturnStatement(_) => true,
        Statement::IfStatement(is) => {
            is.branches.iter().any(|b| stmts_have_return(&b.body))
                || is
                    .else_branch
                    .as_ref()
                    .is_some_and(|e| stmts_have_return(e))
        }
        Statement::WhileStatement(ws) => stmts_have_return(&ws.body),
        Statement::ForStatement(fs) => stmts_have_return(&fs.body),
        Statement::CaseStatement(cs) => {
            cs.when_branches.iter().any(|b| stmts_have_return(&b.body))
                || cs
                    .default_branch
                    .as_ref()
                    .is_some_and(|d| stmts_have_return(d))
        }
        Statement::TryStatement(ts) => {
            stmts_have_return(&ts.try_body)
                || stmts_have_return(&ts.catch_body)
                || ts
                    .finally_body
                    .as_ref()
                    .is_some_and(|f| stmts_have_return(f))
        }
        _ => false,
    }
}

fn stmt_has_user_call(stmt: &Statement, fns: &AsyncResolve<'_>) -> bool {
    // `let/var x = <offloadable extern call>` is a boundary suspension point
    // (plan 101 U8). Detected positionally so nested extern calls (e.g. a fast
    // `sqlite3_column_int(...)` inside another expression) stay sync-inline.
    if let Some((_, v)) = single_binding(stmt) {
        if offloadable_extern_call_args(v).is_some() {
            return true;
        }
    }
    let value = match stmt {
        Statement::LetStatement(ls) => Some(&ls.value),
        Statement::VarStatement(vs) => Some(&vs.value),
        Statement::AssignmentStatement(a) => Some(&a.value),
        Statement::ExpressionStatement(es) => Some(&es.expression),
        Statement::ReturnStatement(rs) => rs.value.as_ref(),
        Statement::ThrowStatement(ts) => Some(&ts.expression),
        Statement::NowaitStatement(nw) => Some(&nw.expression),
        _ => None,
    };
    if let Some(e) = value {
        if expr_has_user_call(e, fns) {
            return true;
        }
    }
    match stmt {
        Statement::IfStatement(is) => {
            is.branches.iter().any(|b| {
                // A branch *condition* can itself contain an async call (it gets
                // hoisted into a preceding `let` by the ANF). Count it, or the
                // suspension would be invisible until after hoisting moved it into
                // the body — too late for the for→while desugar to fire.
                expr_has_user_call(&b.condition, fns) || stmts_have_user_call(&b.body, fns)
            }) || is
                .else_branch
                .as_ref()
                .is_some_and(|e| stmts_have_user_call(e, fns))
        }
        Statement::WhileStatement(ws) => {
            expr_has_user_call(&ws.condition, fns) || stmts_have_user_call(&ws.body, fns)
        }
        Statement::ForStatement(fs) => {
            expr_has_user_call(&fs.items, fns) || stmts_have_user_call(&fs.body, fns)
        }
        Statement::TryStatement(ts) => {
            stmts_have_user_call(&ts.try_body, fns)
                || stmts_have_user_call(&ts.catch_body, fns)
                || ts
                    .finally_body
                    .as_ref()
                    .is_some_and(|f| stmts_have_user_call(f, fns))
        }
        _ => false,
    }
}

/// Whether `expr` contains a user-function call anywhere (used to reject
/// nested awaits the v1 lowering can't place, e.g. `print(child())`).
fn expr_has_user_call(expr: &Expression, fns: &AsyncResolve<'_>) -> bool {
    if user_callee(expr, fns).is_some() {
        return true;
    }
    // `remoteCall(...)` is a suspension point too (lowered as `Term::AwaitRemote`),
    // so it counts as "has a call that needs segment handling" — a statement
    // containing one (other than at a directly-handled position) can't be pushed
    // inline as a plain sync segment statement.
    if remote_call_args(expr).is_some() {
        return true;
    }
    if host_op_call_args(expr, fns).is_some() {
        return true;
    }
    match expr {
        Expression::CallExpression(c) => {
            expr_has_user_call(&c.callee, fns)
                || c.args.iter().any(|a| expr_has_user_call(&a.value, fns))
        }
        Expression::BinaryExpression(b) => {
            expr_has_user_call(&b.left, fns) || expr_has_user_call(&b.right, fns)
        }
        Expression::UnaryExpression(u) => expr_has_user_call(&u.expression, fns),
        Expression::MemberExpression(m) => expr_has_user_call(&m.object, fns),
        Expression::IndexExpression(i) => {
            expr_has_user_call(&i.object, fns) || expr_has_user_call(&i.index, fns)
        }
        Expression::OptionalCheckExpression(o) => expr_has_user_call(&o.expression, fns),
        Expression::ForceUnwrapExpression(f) => expr_has_user_call(&f.expression, fns),
        Expression::ArrayExpression(a) => a.items.iter().any(|it| expr_has_user_call(it, fns)),
        Expression::TupleExpression(t) => t.items.iter().any(|it| expr_has_user_call(it, fns)),
        Expression::DictionaryExpression(d) => d
            .entries
            .iter()
            .any(|entry| expr_has_user_call(&entry.value, fns)),
        Expression::RangeExpression(r) => {
            expr_has_user_call(&r.start, fns) || expr_has_user_call(&r.end, fns)
        }
        _ => false,
    }
}

/// A single-binding `let`/`var` statement → `(name, value)`.
fn single_binding<'a>(stmt: &'a Statement) -> Option<(&'a str, &'a Expression)> {
    match stmt {
        Statement::LetStatement(ls) if ls.bindings.len() == 1 => {
            Some((ls.bindings[0].name.as_str(), &ls.value))
        }
        Statement::VarStatement(vs) if vs.bindings.len() == 1 => {
            Some((vs.bindings[0].name.as_str(), &vs.value))
        }
        _ => None,
    }
}

/// Detect the `let name Type = from_dict(dict)` form (single binding, typed
/// LHS, one-arg `from_dict(...)` RHS), returning `(name, type_name, dict_expr)`.
///
/// `from_dict` has no expression-level lowering — it is expanded at the
/// statement level using the LHS type annotation, which `single_binding`
/// discards. The sync `compile_let_statement` does this directly; the async
/// resume-fn segment compiler needs the same recognition so an async function
/// containing `from_dict` (common once a body is async-colored, e.g. an
/// `http:beforeRequest` listener doing `from_dict(e.data)`) doesn't fall
/// through to `UnknownIdentifier("from_dict")`.
fn from_dict_binding<'a>(stmt: &'a Statement) -> Option<(&'a str, &'a str, &'a Expression)> {
    let (name, type_annotation, value) = match stmt {
        Statement::LetStatement(ls) if ls.bindings.len() == 1 => (
            ls.bindings[0].name.as_str(),
            ls.bindings[0].type_name.as_ref(),
            &ls.value,
        ),
        Statement::VarStatement(vs) if vs.bindings.len() == 1 => (
            vs.bindings[0].name.as_str(),
            vs.bindings[0].type_name.as_ref(),
            &vs.value,
        ),
        _ => return None,
    };
    let type_name = type_annotation?.name.as_deref()?;
    let Expression::CallExpression(ce) = value else {
        return None;
    };
    let Expression::IdentifierExpression(id) = &*ce.callee else {
        return None;
    };
    if id.name != "from_dict" || ce.args.len() != 1 {
        return None;
    }
    Some((name, type_name, &ce.args[0].value))
}

/// If `expr` is `all(c1(), c2(), ...)` where every argument is a user-call,
/// return the list of `(callee, args)` children. `all` is a builtin keyword,
/// not a user function.
type AllChild<'a> = (
    String,
    Vec<AsyncCallArg<'a>>,
    &'a fai_compiler::ast::SourceLocation,
);

fn all_call<'a>(expr: &'a Expression, fns: &AsyncResolve<'_>) -> Option<Vec<AllChild<'a>>> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::IdentifierExpression(id) = &*call.callee else {
        return None;
    };
    if id.name != "all" || call.args.is_empty() {
        return None;
    }
    let mut children = Vec::with_capacity(call.args.len());
    for a in &call.args {
        let (callee, args, loc) = user_callee(&a.value, fns)?;
        // No nested user calls in a child's own args (v1).
        if args.iter().any(|x| expr_has_user_call(x.value, fns)) {
            return None;
        }
        children.push((callee, args, loc));
    }
    Some(children)
}

/// What a block does at entry with a prior suspension's children. A block
/// resumed after an `await`/`all` first checks each child for failure
/// (propagating the error if any failed), then binds the named results.
enum Incoming {
    None,
    /// One entry per awaited child (1 for an await, N for `all`). `Some(name)`
    /// binds child k's result to that local; `None` discards it. `on_error`
    /// is the enclosing catch handler `(catch block, error binding)` if the
    /// await was inside a `try`; a failed child jumps there instead of
    /// failing the task.
    Awaited {
        binds: Vec<Option<String>>,
        on_error: Option<(usize, String)>,
    },
    /// Resume of an `AwaitRemote`: bind `remote_result(g_current)` to `bind`
    /// (or discard if `None`). `on_error` is the enclosing catch handler if
    /// the remote call was inside a `try`; a failed RPC jumps there instead of
    /// failing the task.
    AwaitedRemote {
        bind: Option<String>,
        on_error: Option<(usize, String)>,
    },
    /// Resume of an `AwaitFfi`: bind `ffi_result(g_current)` to `bind`
    /// (or discard if `None`).
    AwaitedFfi {
        bind: Option<String>,
    },
    /// Resume of a generic blocking host operation: bind
    /// `host_op_result(g_current)` to `bind` (or discard if `None`).
    #[allow(dead_code)]
    AwaitedHostOp {
        bind: Option<String>,
        on_error: Option<(usize, String)>,
    },
}

/// A basic block in the resumable function's CFG: what to assign at entry
/// (from a prior suspension's results), the non-suspending statements to
/// run, and a terminator. The block index is the `resume_state` value used
/// to dispatch to it; control flows between blocks by setting `resume_state`
/// and re-dispatching (jumps) or returning to the scheduler (suspensions).
struct Block<'a> {
    incoming: Incoming,
    stmts: Vec<&'a Statement>,
    term: Term<'a>,
    /// Enclosing async catch handler for non-suspending statements in this
    /// block. Sync helper calls compiled inside async resume segments must
    /// route `error_flag` through the scheduler instead of returning from the
    /// resume function.
    on_error: Option<(usize, String)>,
    /// Phase 3 reclamation (plans/111): frame-var names to `rt_drop` after this
    /// block's statements run, before its terminator. Set on the back-edge
    /// block of a NON-suspending `while` body to free confined fresh-literal
    /// loop-body temporaries each iteration (the CFG path's analogue of the
    /// sync Builder's per-iteration `pop_scope` drops).
    drops: Vec<String>,
}

enum Term<'a> {
    /// Placeholder while the CFG is being built; must be replaced.
    Unset,
    /// Unconditional jump: set `resume_state` and re-dispatch.
    Goto(usize),
    /// Branch on a (non-suspending) condition.
    Cond {
        cond: &'a Expression,
        then_blk: usize,
        else_blk: usize,
    },
    /// `sleep(ms)` then resume at `next`.
    Sleep { ms: &'a Expression, next: usize },
    /// `remoteCall(url, fn, args, hash)` — the RPC client transport. Lowered as
    /// a suspending host op: `remote_begin(g_current, …)` starts the request and
    /// parks the task; on resume the next block binds the response via
    /// `remote_result(g_current)`. Browser does the request with async `fetch`,
    /// so the UI thread stays free while it's in flight.
    AwaitRemote {
        args: Vec<&'a Expression>,
        next: usize,
    },
    /// `let x = externCall(args)` for an offloadable (scalar) extern — lowered
    /// as a suspending host op (plan 101 U8): `ffi_begin(g_current, ext_idx,
    /// count, args_buf)` offloads the blocking C call to the boundary and parks
    /// the task; on resume the next block binds the value via
    /// `ffi_result(g_current)`.
    AwaitFfi {
        ext_idx: u16,
        args: Vec<&'a Expression>,
        next: usize,
    },
    /// Generic blocking stdlib host operation. `host_op_begin(g_current,
    /// op_kind, count, args_buf)` copies owned inputs on the host side,
    /// offloads work to the boundary, and parks this task. The next block
    /// reads the value with `host_op_result(g_current)`.
    #[allow(dead_code)]
    AwaitHostOp {
        op_kind: i32,
        args: Vec<&'a Expression>,
        loc: &'a fai_compiler::ast::SourceLocation,
        next: usize,
    },
    /// `await callee(args)` then resume at `next` (which binds the result).
    Await {
        callee: String,
        args: Vec<AsyncCallArg<'a>>,
        /// Call-site location for generic type-arg lookup.
        loc: &'a fai_compiler::ast::SourceLocation,
        next: usize,
    },
    /// `all(c1(), c2(), ...)` — spawn each, join on all, resume at `next`.
    All {
        children: Vec<AllChild<'a>>,
        next: usize,
    },
    /// `await` of an async *closure* call — `closure` is an expression that
    /// evaluates to a closure value (a `do…end` literal for now). Spawned via
    /// its heap header (frame size + table slot), then awaited like a named
    /// child; resume at `next`.
    AwaitClosure {
        closure: &'a Expression,
        args: Vec<&'a Expression>,
        next: usize,
    },
    /// Complete the task with an expression value.
    Complete(&'a Expression),
    /// Complete the task with `Void`.
    CompleteVoid,
    /// Complete with the result of the just-awaited child in pending slot 0
    /// (an await in tail/return position).
    CompletePending,
    /// Complete the task with `remote_result(g_current)` — a `remoteCall(...)`
    /// in tail/return position (the generated RPC client stubs return it).
    CompleteRemote { on_error: Option<(usize, String)> },
    /// Complete the task with `host_op_result(g_current)` — a generic host op
    /// in tail/return position.
    CompleteHostOp { on_error: Option<(usize, String)> },
    /// `throw value` inside a `try`: bind the value to the catch handler's
    /// name and jump to the catch block.
    ThrowTo {
        value: &'a Expression,
        catch_blk: usize,
        err_var: String,
    },
    /// `throw value` with no enclosing `try`: fail the task with the value.
    Fail(&'a Expression),
    /// Store `value` into the try-result local, then jump to `next`. Used to
    /// carry a `try`/`catch` body's value to a `finally` before completing.
    StoreResultGoto { value: &'a Expression, next: usize },
    /// Complete the task with the try-result local (after `finally` ran).
    CompleteResult,
}

/// How the last statement of a lowered sequence yields its value.
#[derive(Clone, Copy)]
enum TailMode {
    /// Not a tail sequence — control continues after it.
    None,
    /// Tail: complete the task with the value.
    Complete,
    /// Tail-in-a-try-with-finally: store the value into the try-result local
    /// and jump to `next` (the finally block).
    StoreResult(usize),
}

/// Result of lowering a statement/sequence: control continues at a block,
/// or the path diverged (completed/returned).
enum Flow {
    Continue(usize),
    Diverged,
}

/// Frame layout for one async function: a heap block holding each
/// param/local (i64 slots, in declaration order) followed by a pending
/// region of `pending_count` i32 child-id slots used to remember awaited
/// tasks (one for an auto-await, N for an `all(...)`) between segments.
pub(super) struct AsyncFrame {
    var_off: std::collections::HashMap<String, u64>,
    vars: Vec<String>,
    pending_off: u64,
    pub(super) size: i32,
    /// Closures reserve frame slot 0 for the captured-env address (`env_ptr`);
    /// the resume fn seeds `__env_ptr` from it at each entry so upvalue reads
    /// (`__env_ptr + i*8`) work. Named fns have no env slot.
    has_env: bool,
}

fn push_unique(vars: &mut Vec<String>, name: &str) {
    if !vars.iter().any(|v| v == name) {
        vars.push(name.to_string());
    }
}

/// Collect every `let`/`var` binding name, descending into `if`/`while`/
/// `try` bodies. Deduped (a name shared across sibling branches gets one
/// frame slot).
fn collect_async_vars(stmts: &[Statement], vars: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            Statement::LetStatement(ls) => {
                for b in &ls.bindings {
                    push_unique(vars, &b.name);
                }
            }
            Statement::VarStatement(vs) => {
                for b in &vs.bindings {
                    push_unique(vars, &b.name);
                }
            }
            Statement::IfStatement(is) => {
                for branch in &is.branches {
                    collect_async_vars(&branch.body, vars);
                }
                if let Some(e) = &is.else_branch {
                    collect_async_vars(e, vars);
                }
            }
            Statement::WhileStatement(ws) => collect_async_vars(&ws.body, vars),
            Statement::ForStatement(fs) => collect_async_vars(&fs.body, vars),
            Statement::TryStatement(ts) => {
                push_unique(vars, &ts.catch_name);
                collect_async_vars(&ts.try_body, vars);
                collect_async_vars(&ts.catch_body, vars);
                if let Some(f) = &ts.finally_body {
                    collect_async_vars(f, vars);
                }
            }
            _ => {}
        }
    }
}

/// Collect the names rebound by a MULTI-variable assignment (`x, y = expr`)
/// anywhere in `stmts`, descending into nested bodies. Those names are
/// EXCLUDED from the async completion release set: the tuple-destructure
/// assignment path plain-overwrites without retain-new / release-old, so the
/// slot's reference count is not guaranteed `+1` at completion.
///
/// SINGLE-variable reassignment (`x = expr`) is no longer an exclusion:
/// `build_resume_fn` marks those frame locals owned (`owned_frame_locals`),
/// so `compile_assignment` maintains the `+1` (retain-new / release-old) and
/// completion can release them — this is what stops `html = html + piece`
/// accumulators leaking every intermediate (the brain SSR leak, plan 116).
/// Field/index mutations (`x.f = …`, `x[i] = …`) don't rebind `x` — they
/// mutate its contents, so `x` keeps its single owned ref and is NOT
/// collected here.
fn collect_multi_rebound_names(stmts: &[Statement], out: &mut std::collections::HashSet<String>) {
    for stmt in stmts {
        match stmt {
            Statement::AssignmentStatement(a) => {
                if let fai_compiler::ast::AssignmentTarget::Variables { names } = &a.target {
                    if names.len() > 1 {
                        for n in names {
                            out.insert(n.clone());
                        }
                    }
                }
            }
            Statement::IfStatement(is) => {
                for branch in &is.branches {
                    collect_multi_rebound_names(&branch.body, out);
                }
                if let Some(e) = &is.else_branch {
                    collect_multi_rebound_names(e, out);
                }
            }
            Statement::WhileStatement(ws) => collect_multi_rebound_names(&ws.body, out),
            Statement::ForStatement(fs) => collect_multi_rebound_names(&fs.body, out),
            Statement::TryStatement(ts) => {
                collect_multi_rebound_names(&ts.try_body, out);
                collect_multi_rebound_names(&ts.catch_body, out);
                if let Some(f) = &ts.finally_body {
                    collect_multi_rebound_names(f, out);
                }
            }
            _ => {}
        }
    }
}

/// Collect every `try ... catch e` binding name in `stmts` (recursively). Catch
/// vars are EXCLUDED from the async completion release set: a `throw <borrowed>`
/// (`Term::ThrowTo`) binds the catch var to a borrowed value (rc not `+1`), so
/// releasing it could over-release. Conservative — catch payloads are small.
fn collect_catch_names(stmts: &[Statement], out: &mut std::collections::HashSet<String>) {
    for stmt in stmts {
        match stmt {
            Statement::IfStatement(is) => {
                for branch in &is.branches {
                    collect_catch_names(&branch.body, out);
                }
                if let Some(e) = &is.else_branch {
                    collect_catch_names(e, out);
                }
            }
            Statement::WhileStatement(ws) => collect_catch_names(&ws.body, out),
            Statement::ForStatement(fs) => collect_catch_names(&fs.body, out),
            Statement::TryStatement(ts) => {
                out.insert(ts.catch_name.clone());
                collect_catch_names(&ts.try_body, out);
                collect_catch_names(&ts.catch_body, out);
                if let Some(f) = &ts.finally_body {
                    collect_catch_names(f, out);
                }
            }
            _ => {}
        }
    }
}

/// Collect the names of functions that are *spawned* anywhere in `stmts`:
/// `nowait f(...)` targets and `all(f(...), g(...))` children. A spawned
/// function must be a resume task (it lives in the function table), so it
/// has to join the async set even when its own body never suspends.
fn collect_spawn_targets(
    stmts: &[Statement],
    r: &AsyncResolve<'_>,
    out: &mut std::collections::HashSet<String>,
) {
    fn from_call(
        expr: &Expression,
        r: &AsyncResolve<'_>,
        out: &mut std::collections::HashSet<String>,
    ) {
        if let Expression::CallExpression(c) = expr {
            if let Some(n) = r.resolve_call(c) {
                out.insert(n);
            }
        }
    }
    fn from_all(
        expr: &Expression,
        r: &AsyncResolve<'_>,
        out: &mut std::collections::HashSet<String>,
    ) {
        if let Expression::CallExpression(c) = expr {
            if let Expression::IdentifierExpression(id) = &*c.callee {
                if id.name == "all" {
                    for a in &c.args {
                        from_call(&a.value, r, out);
                    }
                }
            }
        }
    }
    for stmt in stmts {
        match stmt {
            Statement::NowaitStatement(nw) => from_call(&nw.expression, r, out),
            Statement::LetStatement(ls) => from_all(&ls.value, r, out),
            Statement::VarStatement(vs) => from_all(&vs.value, r, out),
            Statement::IfStatement(is) => {
                for branch in &is.branches {
                    collect_spawn_targets(&branch.body, r, out);
                }
                if let Some(e) = &is.else_branch {
                    collect_spawn_targets(e, r, out);
                }
            }
            Statement::WhileStatement(ws) => collect_spawn_targets(&ws.body, r, out),
            Statement::ForStatement(fs) => collect_spawn_targets(&fs.body, r, out),
            Statement::TryStatement(ts) => {
                collect_spawn_targets(&ts.try_body, r, out);
                collect_spawn_targets(&ts.catch_body, r, out);
                if let Some(f) = &ts.finally_body {
                    collect_spawn_targets(f, r, out);
                }
            }
            _ => {}
        }
    }
}

/// Max child-id slots a statement (and its nested bodies) need across a
/// suspension: `all(...)` → its arg count, a single await → 1, else 0.
fn stmt_pending_count(
    stmt: &Statement,
    fns: &AsyncResolve<'_>,
    params: &std::collections::HashSet<String>,
) -> usize {
    let value = match stmt {
        Statement::LetStatement(ls) => Some(&ls.value),
        Statement::VarStatement(vs) => Some(&vs.value),
        Statement::ExpressionStatement(es) => Some(&es.expression),
        Statement::ReturnStatement(rs) => rs.value.as_ref(),
        _ => None,
    };
    let mut m = match value {
        Some(v) if all_call(v, fns).is_some() => all_call(v, fns).unwrap().len(),
        Some(v) if user_callee(v, fns).is_some() => 1,
        // A closure-parameter / computed-callee call lowers to `Term::AwaitClosure`,
        // which (in both its sync and async sub-paths) writes a child/synth task id
        // to `frame[pending_off]`. Without counting it the pending region is unsized
        // and that write overflows the frame into the adjacent heap object. (This is
        // what silently lost `children()`'s captured locals in forui.)
        Some(v) if indirect_closure_call(v, params, fns).is_some() => 1,
        _ => 0,
    };
    let recur = |stmts: &[Statement], m: &mut usize| {
        for s in stmts {
            *m = (*m).max(stmt_pending_count(s, fns, params));
        }
    };
    match stmt {
        Statement::IfStatement(is) => {
            for branch in &is.branches {
                recur(&branch.body, &mut m);
            }
            if let Some(e) = &is.else_branch {
                recur(e, &mut m);
            }
        }
        Statement::WhileStatement(ws) => recur(&ws.body, &mut m),
        Statement::ForStatement(fs) => recur(&fs.body, &mut m),
        Statement::TryStatement(ts) => {
            recur(&ts.try_body, &mut m);
            recur(&ts.catch_body, &mut m);
            if let Some(f) = &ts.finally_body {
                recur(f, &mut m);
            }
        }
        _ => {}
    }
    m
}

/// Compute the frame layout: params first, then every `let`/`var` binding
/// name (recursively, including nested control flow and multi-binding
/// `let a, b = all(...)`), plus a pending region sized to the widest
/// suspension.
pub(super) fn async_frame_layout(
    fd: &FunctionDeclaration,
    fns: &AsyncResolve<'_>,
    has_env: bool,
) -> AsyncFrame {
    let mut vars: Vec<String> = Vec::new();
    // Hidden `@type` params lead the frame (matching the sync ABI's leading
    // type-arg slots) so a generic callee's params land at the right offsets.
    for t in &fd.type_params {
        push_unique(&mut vars, &t.name);
    }
    for p in &fd.params {
        push_unique(&mut vars, &p.name);
    }
    collect_async_vars(&fd.body, &mut vars);
    // Param names — `stmt_pending_count` uses these to recognize a closure-param
    // call (`p()`) as a suspension that needs a pending slot, mirroring the CFG's
    // `indirect_closure_call` detection.
    let params: std::collections::HashSet<String> =
        fd.params.iter().map(|p| p.name.clone()).collect();
    let mut pending_count = 0usize;
    for stmt in &fd.body {
        pending_count = pending_count.max(stmt_pending_count(stmt, fns, &params));
    }
    // Closures reserve slot 0 for `env_ptr`; named-fn vars start at 0.
    let base: u64 = if has_env { 8 } else { 0 };
    let mut var_off = std::collections::HashMap::new();
    for (i, v) in vars.iter().enumerate() {
        var_off.insert(v.clone(), base + (i as u64) * 8);
    }
    let pending_off = base + (vars.len() as u64) * 8;
    // Value slots (i64) + pending region (i32 each), rounded up to 8 bytes.
    let raw = pending_off as usize + pending_count * 4;
    let size = ((raw + 7) & !7) as i32;
    AsyncFrame {
        var_off,
        vars,
        pending_off,
        size: size.max(8),
        has_env,
    }
}

/// Builds the CFG for an async function body. Lowers structured control
/// flow (`if`/`while`) and suspensions into basic blocks connected by
/// `Goto`/`Cond`/`Sleep`/`Await`/`All`/`Complete*`. Returns `None` (→ fall
/// back) for anything out of v1 scope.
struct CfgBuilder<'a> {
    blocks: Vec<Block<'a>>,
    fns: &'a AsyncResolve<'a>,
    /// Enclosing `try` handlers (catch block, catch binding name), innermost
    /// last. A `throw` targets the top of the stack.
    handlers: Vec<(usize, String)>,
    /// Closure-typed parameter names of the function being lowered. A call
    /// `p(...)` whose callee is one of these is an indirect closure call (it
    /// may suspend) → an await with runtime sync/async dispatch.
    params: &'a std::collections::HashSet<String>,
    /// Conservative escaping-name set for the whole function — used to pick
    /// confined fresh-literal loop-body temporaries to drop per iteration.
    escaping: &'a std::collections::HashSet<String>,
}

impl<'a> CfgBuilder<'a> {
    fn new_block(&mut self) -> usize {
        self.blocks.push(Block {
            incoming: Incoming::None,
            stmts: Vec::new(),
            term: Term::Unset,
            on_error: None,
            drops: Vec::new(),
        });
        self.blocks.len() - 1
    }

    fn push_inline_stmt(&mut self, blk: usize, stmt: &'a Statement) -> Result<(), ()> {
        let handler = self.handler();
        if self.blocks[blk].on_error.is_none() {
            self.blocks[blk].on_error = handler.clone();
        } else if self.blocks[blk].on_error != handler {
            return Err(());
        }
        self.blocks[blk].stmts.push(stmt);
        Ok(())
    }

    fn args_ok(&self, args: &[&Expression]) -> bool {
        !args.iter().any(|a| expr_has_user_call(a, self.fns))
    }

    fn async_args_ok(&self, args: &[AsyncCallArg<'a>]) -> bool {
        !args.iter().any(|a| expr_has_user_call(a.value, self.fns))
    }

    /// Lower `stmts` starting at `entry`. `is_tail` marks that the last
    /// statement's value is the function's result. Returns where control
    /// continues, or `Diverged` if the sequence always completes/returns.
    fn lower_seq(
        &mut self,
        stmts: &'a [Statement],
        entry: usize,
        mode: TailMode,
    ) -> Result<Flow, ()> {
        let n = stmts.len();
        if n == 0 {
            return self.finish_void(entry, mode);
        }
        let mut cur = entry;
        for (i, stmt) in stmts.iter().enumerate() {
            let m = if i + 1 == n { mode } else { TailMode::None };
            match self.lower_stmt(stmt, cur, m)? {
                Flow::Continue(next) => cur = next,
                Flow::Diverged => return Ok(Flow::Diverged),
            }
        }
        // The last statement fell through (it produced no value) — in tail
        // position the function's value is `Void`.
        self.finish_void(cur, mode)
    }

    /// Terminate `blk` for a sequence that fell through (no value) per `mode`.
    fn finish_void(&mut self, blk: usize, mode: TailMode) -> Result<Flow, ()> {
        match mode {
            TailMode::None => Ok(Flow::Continue(blk)),
            TailMode::Complete => {
                self.blocks[blk].term = Term::CompleteVoid;
                Ok(Flow::Diverged)
            }
            // A `Void` value flowing into a try-with-finally result slot is
            // out of v1 scope (the result is held in a wasm local).
            TailMode::StoreResult(_) => Err(()),
        }
    }

    /// Terminate `blk` producing `value` per `mode` (must be a tail mode).
    fn tail_value(&mut self, blk: usize, value: &'a Expression, mode: TailMode) -> Flow {
        match mode {
            TailMode::Complete => self.blocks[blk].term = Term::Complete(value),
            TailMode::StoreResult(next) => {
                self.blocks[blk].term = Term::StoreResultGoto { value, next }
            }
            TailMode::None => unreachable!("tail_value with TailMode::None"),
        }
        Flow::Diverged
    }

    /// The enclosing catch handler `(catch block, error binding)`, if any.
    fn handler(&self) -> Option<(usize, String)> {
        self.handlers.last().map(|(b, n)| (*b, n.clone()))
    }

    fn lower_stmt(&mut self, stmt: &'a Statement, cur: usize, mode: TailMode) -> Result<Flow, ()> {
        let is_tail = !matches!(mode, TailMode::None);
        // sleep(ms)
        if let Some(ms) = async_sleep_arg_of(stmt) {
            let next = self.new_block();
            self.blocks[cur].term = Term::Sleep { ms, next };
            return Ok(Flow::Continue(next));
        }
        // nowait userCall(...) — in-segment fork
        if let Statement::NowaitStatement(nw) = stmt {
            let Some((_, args, _)) = user_callee(&nw.expression, self.fns) else {
                return Err(());
            };
            if !self.async_args_ok(&args) {
                return Err(());
            }
            self.push_inline_stmt(cur, stmt)?;
            return Ok(Flow::Continue(cur));
        }
        // let/var [a, b] = all(...)
        if matches!(
            stmt,
            Statement::LetStatement(_) | Statement::VarStatement(_)
        ) {
            let (value, binds): (&Expression, Vec<String>) = match stmt {
                Statement::LetStatement(ls) => (
                    &ls.value,
                    ls.bindings.iter().map(|b| b.name.clone()).collect(),
                ),
                Statement::VarStatement(vs) => (
                    &vs.value,
                    vs.bindings.iter().map(|b| b.name.clone()).collect(),
                ),
                _ => unreachable!(),
            };
            if let Some(children) = all_call(value, self.fns) {
                if binds.len() != children.len() {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::All { children, next };
                self.blocks[next].incoming = Incoming::Awaited {
                    binds: binds.into_iter().map(Some).collect(),
                    on_error,
                };
                return Ok(Flow::Continue(next));
            }
        }
        // single-binding let/var
        if let Some((name, value)) = single_binding(stmt) {
            // `let x = remoteCall(...)` — suspend on the RPC, bind the result.
            if let Some((rargs, _loc)) = remote_call_args(value) {
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitRemote {
                    args: rargs,
                    next,
                };
                self.blocks[next].incoming = Incoming::AwaitedRemote {
                    bind: Some(name.to_string()),
                    on_error,
                };
                return Ok(Flow::Continue(next));
            }
            // `let x = request.get(...)` / `post(...)` — suspend on the
            // blocking host HTTP operation and bind the response dict (or null)
            // after the worker completes.
            if let Some((op_kind, hargs, loc)) = host_op_call_args(value, self.fns) {
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitHostOp {
                    op_kind,
                    args: hargs,
                    loc,
                    next,
                };
                let bind = if name == "_" {
                    None
                } else {
                    Some(name.to_string())
                };
                self.blocks[next].incoming = Incoming::AwaitedHostOp { bind, on_error };
                return Ok(Flow::Continue(next));
            }
            // `let x = externCall(...)` for an offloadable scalar extern —
            // offload the blocking C call to the boundary and bind on resume
            // (plan 101 U8). `let _ =` discards.
            if let Some((ext_idx, fargs, _loc)) = offloadable_extern_call_args(value) {
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitFfi {
                    ext_idx,
                    args: fargs,
                    next,
                };
                let bind = if name == "_" {
                    None
                } else {
                    Some(name.to_string())
                };
                self.blocks[next].incoming = Incoming::AwaitedFfi { bind };
                return Ok(Flow::Continue(next));
            }
            if let Some((callee, args, loc)) = user_callee(value, self.fns) {
                if !self.async_args_ok(&args) {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::Await {
                    callee,
                    args,
                    loc,
                    next,
                };
                self.blocks[next].incoming = Incoming::Awaited {
                    binds: vec![Some(name.to_string())],
                    on_error,
                };
                return Ok(Flow::Continue(next));
            }
            if let Some((closure, args)) = async_closure_call(value, self.fns)
                .or_else(|| indirect_closure_call(value, self.params, self.fns))
            {
                if !self.args_ok(&args) {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitClosure {
                    closure,
                    args,
                    next,
                };
                self.blocks[next].incoming = Incoming::Awaited {
                    binds: vec![Some(name.to_string())],
                    on_error,
                };
                return Ok(Flow::Continue(next));
            }
            if expr_has_user_call(value, self.fns) {
                return Err(());
            }
            self.push_inline_stmt(cur, stmt)?;
            return Ok(Flow::Continue(cur));
        }
        // assignment `v = expr` (no awaits in the value)
        if let Statement::AssignmentStatement(asg) = stmt {
            // `x = externCall(...)` (offloadable extern, single-var target) —
            // offload + bind the result back into the existing local on resume
            // (plan 101 U10: the per-row `step = sqlite3_step(stmt)` reassignment
            // in a collect loop). Mirrors the let/var binding case.
            if let AssignmentTarget::Variables { names } = &asg.target {
                if names.len() == 1 {
                    if let Some((op_kind, hargs, loc)) = host_op_call_args(&asg.value, self.fns) {
                        let on_error = self.handler();
                        let next = self.new_block();
                        self.blocks[cur].term = Term::AwaitHostOp {
                            op_kind,
                            args: hargs,
                            loc,
                            next,
                        };
                        self.blocks[next].incoming = Incoming::AwaitedHostOp {
                            bind: Some(names[0].clone()),
                            on_error,
                        };
                        return Ok(Flow::Continue(next));
                    }
                    if let Some((ext_idx, fargs, _loc)) = offloadable_extern_call_args(&asg.value) {
                        let next = self.new_block();
                        self.blocks[cur].term = Term::AwaitFfi {
                            ext_idx,
                            args: fargs,
                            next,
                        };
                        self.blocks[next].incoming = Incoming::AwaitedFfi {
                            bind: Some(names[0].clone()),
                        };
                        return Ok(Flow::Continue(next));
                    }
                }
            }
            if expr_has_user_call(&asg.value, self.fns) {
                return Err(());
            }
            self.push_inline_stmt(cur, stmt)?;
            return Ok(Flow::Continue(cur));
        }
        // expression statement
        if let Statement::ExpressionStatement(es) = stmt {
            // `remoteCall(...)` as a statement — in tail position the RPC result
            // is the function's value (the generated stubs do exactly this);
            // otherwise it's run for effect and the result discarded.
            if let Some((rargs, _loc)) = remote_call_args(&es.expression) {
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitRemote {
                    args: rargs,
                    next,
                };
                match mode {
                    TailMode::Complete => {
                        self.blocks[next].term = Term::CompleteRemote { on_error };
                        return Ok(Flow::Diverged);
                    }
                    TailMode::StoreResult(_) => return Err(()),
                    TailMode::None => {
                        self.blocks[next].incoming = Incoming::AwaitedRemote {
                            bind: None,
                            on_error,
                        };
                        return Ok(Flow::Continue(next));
                    }
                }
            }
            if let Some((op_kind, hargs, loc)) = host_op_call_args(&es.expression, self.fns) {
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitHostOp {
                    op_kind,
                    args: hargs,
                    loc,
                    next,
                };
                match mode {
                    TailMode::Complete => {
                        self.blocks[next].term = Term::CompleteHostOp { on_error };
                        return Ok(Flow::Diverged);
                    }
                    TailMode::StoreResult(_) => return Err(()),
                    TailMode::None => {
                        self.blocks[next].incoming = Incoming::AwaitedHostOp {
                            bind: None,
                            on_error,
                        };
                        return Ok(Flow::Continue(next));
                    }
                }
            }
            if let Some((callee, args, loc)) = user_callee(&es.expression, self.fns) {
                if !self.async_args_ok(&args) {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::Await {
                    callee,
                    args,
                    loc,
                    next,
                };
                match mode {
                    TailMode::Complete => {
                        self.blocks[next].term = Term::CompletePending;
                        return Ok(Flow::Diverged);
                    }
                    // await-in-tail of a try-with-finally body: out of scope.
                    TailMode::StoreResult(_) => return Err(()),
                    TailMode::None => {
                        self.blocks[next].incoming = Incoming::Awaited {
                            binds: vec![None],
                            on_error,
                        };
                        return Ok(Flow::Continue(next));
                    }
                }
            }
            // Invoking a closure value (`children()`, `handlers[i]()`) — await
            // it through the scheduler rather than the sync re-entrant drive.
            if let Some((closure, args)) = async_closure_call(&es.expression, self.fns)
                .or_else(|| indirect_closure_call(&es.expression, self.params, self.fns))
            {
                if !self.args_ok(&args) {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitClosure {
                    closure,
                    args,
                    next,
                };
                match mode {
                    TailMode::Complete => {
                        self.blocks[next].term = Term::CompletePending;
                        return Ok(Flow::Diverged);
                    }
                    TailMode::StoreResult(_) => return Err(()),
                    TailMode::None => {
                        self.blocks[next].incoming = Incoming::Awaited {
                            binds: vec![None],
                            on_error,
                        };
                        return Ok(Flow::Continue(next));
                    }
                }
            }
            if expr_has_user_call(&es.expression, self.fns) {
                return Err(());
            }
            if is_tail {
                return Ok(self.tail_value(cur, &es.expression, mode));
            }
            self.push_inline_stmt(cur, stmt)?;
            return Ok(Flow::Continue(cur));
        }
        // throw value
        if let Statement::ThrowStatement(ts) = stmt {
            if expr_has_user_call(&ts.expression, self.fns) {
                return Err(()); // no await in a throw value (v1)
            }
            self.blocks[cur].term = match self.handlers.last() {
                Some((catch_blk, err_var)) => Term::ThrowTo {
                    value: &ts.expression,
                    catch_blk: *catch_blk,
                    err_var: err_var.clone(),
                },
                None => Term::Fail(&ts.expression),
            };
            return Ok(Flow::Diverged);
        }
        // try / catch / finally
        if let Statement::TryStatement(ts) = stmt {
            return self.lower_try(ts, cur, mode);
        }
        // return
        if let Statement::ReturnStatement(rs) = stmt {
            match &rs.value {
                Some(v) => {
                    if let Some((rargs, _loc)) = remote_call_args(v) {
                        let on_error = self.handler();
                        let next = self.new_block();
                        self.blocks[cur].term = Term::AwaitRemote {
                            args: rargs,
                            next,
                        };
                        self.blocks[next].term = Term::CompleteRemote { on_error };
                    } else if let Some((op_kind, hargs, loc)) = host_op_call_args(v, self.fns) {
                        let on_error = self.handler();
                        let next = self.new_block();
                        self.blocks[cur].term = Term::AwaitHostOp {
                            op_kind,
                            args: hargs,
                            loc,
                            next,
                        };
                        self.blocks[next].term = Term::CompleteHostOp { on_error };
                    } else if let Some((callee, args, loc)) = user_callee(v, self.fns) {
                        if !self.async_args_ok(&args) {
                            return Err(());
                        }
                        let next = self.new_block();
                        self.blocks[cur].term = Term::Await {
                            callee,
                            args,
                            loc,
                            next,
                        };
                        self.blocks[next].term = Term::CompletePending;
                    } else if let Some((closure, args)) = async_closure_call(v, self.fns)
                        .or_else(|| indirect_closure_call(v, self.params, self.fns))
                    {
                        if !self.args_ok(&args) {
                            return Err(());
                        }
                        let next = self.new_block();
                        self.blocks[cur].term = Term::AwaitClosure {
                            closure,
                            args,
                            next,
                        };
                        self.blocks[next].term = Term::CompletePending;
                    } else {
                        if expr_has_user_call(v, self.fns) {
                            return Err(());
                        }
                        self.blocks[cur].term = Term::Complete(v);
                    }
                }
                None => self.blocks[cur].term = Term::CompleteVoid,
            }
            return Ok(Flow::Diverged);
        }
        // if / if-else (no `elsif` chains in v1; no await in condition)
        if let Statement::IfStatement(is) = stmt {
            if is.branches.len() != 1 {
                return Err(());
            }
            let branch = &is.branches[0];
            let cond = &branch.condition;
            if expr_has_user_call(cond, self.fns) {
                return Err(());
            }
            if is_tail {
                if let Some(else_body) = &is.else_branch {
                    // Each branch produces the value (per `mode`).
                    let then_e = self.new_block();
                    let else_e = self.new_block();
                    self.blocks[cur].term = Term::Cond {
                        cond,
                        then_blk: then_e,
                        else_blk: else_e,
                    };
                    self.lower_seq(&branch.body, then_e, mode)?;
                    self.lower_seq(else_body, else_e, mode)?;
                    Ok(Flow::Diverged)
                } else {
                    // No `else` in tail position: only valid for a `Void`
                    // function — the `then` branch runs for effect and both
                    // paths complete `Void`. (A non-Void fn missing the else
                    // value is a checker error; `finish_void` rejects
                    // `StoreResult` here, falling back.) Lower the branch for
                    // effect, then complete `Void` at the merge.
                    let then_e = self.new_block();
                    let join = self.new_block();
                    self.blocks[cur].term = Term::Cond {
                        cond,
                        then_blk: then_e,
                        else_blk: join,
                    };
                    if let Flow::Continue(te) =
                        self.lower_seq(&branch.body, then_e, TailMode::None)?
                    {
                        self.blocks[te].term = Term::Goto(join);
                    }
                    self.finish_void(join, mode)
                }
            } else {
                let then_e = self.new_block();
                let join = self.new_block();
                let else_e = if is.else_branch.is_some() {
                    self.new_block()
                } else {
                    join
                };
                self.blocks[cur].term = Term::Cond {
                    cond,
                    then_blk: then_e,
                    else_blk: else_e,
                };
                if let Flow::Continue(te) = self.lower_seq(&branch.body, then_e, TailMode::None)? {
                    self.blocks[te].term = Term::Goto(join);
                }
                if let Some(else_body) = &is.else_branch {
                    if let Flow::Continue(ee) = self.lower_seq(else_body, else_e, TailMode::None)? {
                        self.blocks[ee].term = Term::Goto(join);
                    }
                }
                Ok(Flow::Continue(join))
            }
        } else if let Statement::WhileStatement(ws) = stmt {
            if expr_has_user_call(&ws.condition, self.fns) {
                return Err(());
            }
            let header = self.new_block();
            self.blocks[cur].term = Term::Goto(header);
            let body_e = self.new_block();
            let exit = self.new_block();
            self.blocks[header].term = Term::Cond {
                cond: &ws.condition,
                then_blk: body_e,
                else_blk: exit,
            };
            if let Flow::Continue(be) = self.lower_seq(&ws.body, body_e, TailMode::None)? {
                // Per-iteration reclamation: for a NON-suspending body (single
                // straight-line block `be`), free its confined fresh-literal
                // top-level temporaries before looping back. A suspending body
                // spans multiple blocks / may not set a binding on every path,
                // so skip it (sound leak). Inner non-suspending if/case/for are
                // compiled inline and drop via their own scope exits.
                if !stmts_have_suspension(&ws.body, self.fns) {
                    self.blocks[be].drops = fai_compiler::escape_analysis::confined_freeable_names(
                        &ws.body,
                        self.escaping,
                    );
                }
                self.blocks[be].term = Term::Goto(header);
            }
            // A `while` yields no value, so in tail position it completes Void.
            self.finish_void(exit, mode)
        } else if !is_tail
            && !stmts_have_suspension(std::slice::from_ref(stmt), self.fns)
            && !stmt_has_return(stmt)
        {
            // Any other statement (e.g. a `for` loop, `case`) that contains no
            // suspension point runs as a plain inline segment statement — the
            // segment compiler (`compile_stmt`) lowers it directly, exactly as a
            // sync function would. Source-level `return` is also excluded here:
            // in a resume function it must go through scheduler completion, not a
            // raw wasm return. Only statements that themselves suspend or return
            // need CFG segment-splitting (not supported inside all statement
            // shapes yet).
            self.push_inline_stmt(cur, stmt)?;
            Ok(Flow::Continue(cur))
        } else {
            if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
                let (kind, loc) = match stmt {
                    Statement::ForStatement(s) => ("for", Some(&s.location)),
                    Statement::WhileStatement(s) => ("while", Some(&s.location)),
                    Statement::CaseStatement(s) => ("case", Some(&s.location)),
                    Statement::IfStatement(s) => ("if", Some(&s.location)),
                    Statement::AssignmentStatement(s) => ("assignment", Some(&s.location)),
                    Statement::ExpressionStatement(s) => ("expr", Some(&s.location)),
                    Statement::LetStatement(s) => ("let", Some(&s.location)),
                    Statement::VarStatement(s) => ("var", Some(&s.location)),
                    _ => ("other", None),
                };
                let at = loc
                    .map(|l| format!("{}:{}", l.line, l.column))
                    .unwrap_or_else(|| "?".to_string());
                eprintln!(
                    "[async-engine]   CFG bail: unsupported suspending `{}` at {} (is_tail={})",
                    kind, at, is_tail
                );
            }
            Err(()) // unsupported statement
        }
    }

    /// Lower a `try`/`catch`/`finally`. In statement position the bodies run
    /// for effect; in value position (tail) each body produces the result,
    /// carried through `finally` via the try-result local.
    fn lower_try(
        &mut self,
        ts: &'a fai_compiler::ast::TryStatement,
        cur: usize,
        mode: TailMode,
    ) -> Result<Flow, ()> {
        let catch_blk = self.new_block();
        match mode {
            TailMode::None => {
                // Statement position: bodies run for effect; finally runs on
                // both paths; control continues after.
                let after = self.new_block();
                let finally_blk = if ts.finally_body.is_some() {
                    self.new_block()
                } else {
                    after
                };
                self.handlers.push((catch_blk, ts.catch_name.clone()));
                let try_exit = self.lower_seq(&ts.try_body, cur, TailMode::None)?;
                self.handlers.pop();
                if let Flow::Continue(te) = try_exit {
                    self.blocks[te].term = Term::Goto(finally_blk);
                }
                if let Flow::Continue(ce) =
                    self.lower_seq(&ts.catch_body, catch_blk, TailMode::None)?
                {
                    self.blocks[ce].term = Term::Goto(finally_blk);
                }
                if let Some(fb) = &ts.finally_body {
                    if let Flow::Continue(fe) = self.lower_seq(fb, finally_blk, TailMode::None)? {
                        self.blocks[fe].term = Term::Goto(after);
                    }
                }
                Ok(Flow::Continue(after))
            }
            _ if ts.finally_body.is_some() => {
                // Value position with finally: only supported at the function
                // tail (`Complete`) and with a non-suspending finally (the
                // try-result lives in a wasm local across it). One result
                // local ⇒ no nested value-try-with-finally.
                if !matches!(mode, TailMode::Complete) {
                    return Err(());
                }
                let fb = ts.finally_body.as_ref().unwrap();
                if stmts_have_suspension(fb, self.fns) {
                    return Err(());
                }
                let finally_blk = self.new_block();
                // try/catch store their value into the try-result, then finally.
                self.handlers.push((catch_blk, ts.catch_name.clone()));
                self.lower_seq(&ts.try_body, cur, TailMode::StoreResult(finally_blk))?;
                self.handlers.pop();
                self.lower_seq(
                    &ts.catch_body,
                    catch_blk,
                    TailMode::StoreResult(finally_blk),
                )?;
                // finally runs for effect, then completes with the result.
                if let Flow::Continue(fe) = self.lower_seq(fb, finally_blk, TailMode::None)? {
                    self.blocks[fe].term = Term::CompleteResult;
                }
                Ok(Flow::Diverged)
            }
            _ => {
                // Value position, no finally: each body produces the result.
                self.handlers.push((catch_blk, ts.catch_name.clone()));
                self.lower_seq(&ts.try_body, cur, mode)?;
                self.handlers.pop();
                self.lower_seq(&ts.catch_body, catch_blk, mode)?;
                Ok(Flow::Diverged)
            }
        }
    }
}

/// Build the CFG for `body`, or `None` if it uses anything out of v1 scope.
fn build_cfg<'a>(
    body: &'a [Statement],
    fns: &'a AsyncResolve<'a>,
    params: &'a std::collections::HashSet<String>,
    escaping: &'a std::collections::HashSet<String>,
) -> Option<Vec<Block<'a>>> {
    let mut cb = CfgBuilder {
        blocks: vec![Block {
            incoming: Incoming::None,
            stmts: Vec::new(),
            term: Term::Unset,
            on_error: None,
            drops: Vec::new(),
        }],
        fns,
        handlers: Vec::new(),
        params,
        escaping,
    };
    match cb.lower_seq(body, 0, TailMode::Complete).ok()? {
        Flow::Diverged => {}
        // Body fell through without producing a value → complete with Void.
        Flow::Continue(exit) => cb.blocks[exit].term = Term::CompleteVoid,
    }
    if cb.blocks.iter().any(|b| matches!(b.term, Term::Unset)) {
        return None;
    }
    Some(cb.blocks)
}

/// Emit `current_task.resume_state` (an i32) onto the stack.
fn emit_load_current_rstate(b: &mut Builder, layout: &crate::async_engine::SchedLayout) {
    b.emit(Instruction::GlobalGet(layout.g_table_base));
    b.emit(Instruction::GlobalGet(layout.g_current));
    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
    b.emit(Instruction::I32Mul);
    b.emit(Instruction::I32Add);
    b.emit(Instruction::I32Load(mem_off(crate::async_engine::O_RSTATE)));
}

/// Emit `current_task.resume_state = state`.
pub(super) fn emit_store_current_rstate(
    b: &mut Builder,
    layout: &crate::async_engine::SchedLayout,
    state: i32,
) {
    b.emit(Instruction::GlobalGet(layout.g_table_base));
    b.emit(Instruction::GlobalGet(layout.g_current));
    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
    b.emit(Instruction::I32Mul);
    b.emit(Instruction::I32Add);
    b.emit(Instruction::I32Const(state));
    b.emit(Instruction::I32Store(mem_off(
        crate::async_engine::O_RSTATE,
    )));
}

/// Mark the current task as waiting on an external host completion. The host
/// will resume it explicitly, so O_WAKE is set to -1 to avoid timer promotion.
fn emit_park_current_task(b: &mut Builder, layout: &crate::async_engine::SchedLayout) {
    let rec = |b: &mut Builder| {
        b.emit(Instruction::GlobalGet(layout.g_table_base));
        b.emit(Instruction::GlobalGet(layout.g_current));
        b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
        b.emit(Instruction::I32Mul);
        b.emit(Instruction::I32Add);
    };
    rec(b);
    b.emit(Instruction::I32Const(crate::async_engine::ST_WAITING));
    b.emit(Instruction::I32Store(mem_off(
        crate::async_engine::O_STATUS,
    )));
    rec(b);
    b.emit(Instruction::F64Const(-1.0));
    b.emit(Instruction::F64Store(mem_off(crate::async_engine::O_WAKE)));
}

/// Build a temporary argv buffer containing one NaN-boxed i64 per argument.
/// Host begin imports must copy anything they need before returning; callers
/// free this buffer immediately after the begin import.
fn emit_boxed_arg_buffer(
    b: &mut Builder,
    args: &[&Expression],
) -> Result<(u32, i32, Vec<u32>), BuildError> {
    let arg_count = args.len() as i32;
    let byte_len = (arg_count * 8).max(8);
    let buf = b.alloc_i32_local();
    let mut owned_arg_locals = Vec::new();
    b.emit(Instruction::I32Const(byte_len));
    b.emit(Instruction::Call(b.rt().base + crate::runtime::RT_ALLOC));
    b.emit(Instruction::LocalSet(buf));
    for (i, a) in args.iter().enumerate() {
        b.emit(Instruction::LocalGet(buf));
        b.emit(Instruction::I32Const((i as i32) * 8));
        b.emit(Instruction::I32Add);
        let result = b.compile_expr_result_as(a, ValueShape::Boxed)?;
        if result.ownership == ExprOwnership::Owned {
            let owned = b.alloc_local();
            b.emit(Instruction::LocalTee(owned));
            owned_arg_locals.push(owned);
        }
        b.emit(Instruction::I64Store(MemArg {
            offset: 0,
            align: 3,
            memory_index: 0,
        }));
    }
    Ok((buf, byte_len, owned_arg_locals))
}

fn emit_release_owned_arg_locals(b: &mut Builder, locals: &[u32]) {
    for local in locals {
        b.emit(Instruction::LocalGet(*local));
        b.emit_ownership_event_for_stack(OwnershipOp::Release, OWNERSHIP_SITE_UNKNOWN, 0);
        b.emit(Instruction::Call(b.rt().base + RT_RELEASE));
    }
}

/// Emit `frame_ptr_local = current_task.frame`.
fn emit_load_current_frame(
    b: &mut Builder,
    layout: &crate::async_engine::SchedLayout,
    frame_ptr_local: u32,
) {
    b.emit(Instruction::GlobalGet(layout.g_table_base));
    b.emit(Instruction::GlobalGet(layout.g_current));
    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
    b.emit(Instruction::I32Mul);
    b.emit(Instruction::I32Add);
    b.emit(Instruction::I32Load(mem_off(crate::async_engine::O_FRAME)));
    b.emit(Instruction::LocalSet(frame_ptr_local));
}

/// Compile one non-suspending in-segment statement: a single-binding
/// `let`/`var` stores into the binding's frame-backed local; a plain
/// expression statement (e.g. `print`) compiles for its side effect.
fn compile_async_segment_stmt(
    b: &mut Builder,
    stmt: &Statement,
    var_local: &std::collections::HashMap<String, u32>,
    release_set: &std::collections::HashSet<String>,
) -> Result<(), BuildError> {
    // `let name Type = from_dict(dict)` — expand using the LHS type annotation
    // (which `single_binding` drops) exactly as the sync `compile_let_statement`
    // does, then store the materialized record into the async frame slot. Guard
    // on a known type so an unknown annotation falls through to the generic path
    // and reports the same diagnostic the sync path would.
    if let Some((name, type_name, dict_expr)) = from_dict_binding(stmt) {
        if b.ctx.type_fields.contains_key(type_name) {
            let local = *var_local
                .get(name)
                .ok_or(BuildError::UnsupportedExpression("async-unknown-binding"))?;
            b.compile_from_dict_value(type_name, dict_expr)?;
            let is_cell = b.lookup(name).map(|bnd| bnd.is_cell).unwrap_or(false);
            if is_cell {
                b.emit_cell_store(local, ExprResult::boxed(true));
            } else {
                b.assign_to_async_frame_slot(
                    local,
                    ExprResult::boxed(true),
                    release_set.contains(name),
                );
            }
            return Ok(());
        }
    }
    if let Some((name, value)) = single_binding(stmt) {
        let local = *var_local
            .get(name)
            .ok_or(BuildError::UnsupportedExpression("async-unknown-binding"))?;
        // A cell-captured var's binding local holds the heap cell's address
        // (plan 114): store the initial value *through* the cell's value
        // slot with value-RC — a plain `LocalSet` would clobber the address
        // with the value and hand the capturing closure an i64 where it
        // expects an i32.
        let is_cell = b.lookup(name).map(|bnd| bnd.is_cell).unwrap_or(false);
        if is_cell {
            let result = b.compile_expr_result_as(value, ValueShape::Boxed)?;
            b.emit_cell_store(local, result);
        } else {
            let result = b.compile_expr_result_as(value, ValueShape::Boxed)?;
            // Vars NOT in the release set (multi-assign targets, catch vars)
            // keep the no-retain/no-release behaviour — they leak, soundly.
            b.assign_to_async_frame_slot(local, result, release_set.contains(name));
        }
        return Ok(());
    }
    b.compile_stmt(stmt)
}

/// Emit code that spawns a child task for `callee(args)`: allocate the
/// child's frame, write the argument values into its leading param slots,
/// and `spawn` it. Leaves the new task id in `childid_l`.
#[allow(clippy::too_many_arguments)]
fn emit_spawn_child(
    b: &mut Builder,
    callee: &str,
    args: &[AsyncCallArg<'_>],
    loc: &fai_compiler::ast::SourceLocation,
    frame_sizes: &std::collections::HashMap<String, i32>,
    fn_table_idx: &std::collections::HashMap<String, u32>,
    layout: &crate::async_engine::SchedLayout,
    childframe_l: u32,
    childid_l: u32,
) -> Result<(), BuildError> {
    let size = *frame_sizes
        .get(callee)
        .ok_or(BuildError::UnsupportedExpression("async-unknown-callee"))?;
    let tidx = *fn_table_idx
        .get(callee)
        .ok_or(BuildError::UnsupportedExpression("async-unknown-callee"))?;
    // Generic callee? Its frame leads with hidden `@type` slots — interned
    // type-name strings, exactly as `compile_call` injects them for the sync
    // ABI. Look the type args up by the call-site key the checker recorded.
    let tpc = b
        .function_by_name
        .get(callee)
        .map(|&p| b.functions()[p as usize].type_param_count as usize)
        .unwrap_or(0);
    b.emit(Instruction::I32Const(size));
    b.emit(Instruction::Call(layout.alloc));
    b.emit(Instruction::LocalSet(childframe_l));
    // Zero the fresh frame (plan 115): the allocator reuses freed frame blocks
    // without clearing them, so a slot not written on the path to a completion
    // would hold a STALE pointer from the previous task. Async reclamation
    // RT_RELEASEs every owned body slot at completion; zeroing makes an unwritten
    // slot read 0 (a safe RT_RELEASE no-op) instead of double-freeing the prior
    // task's object. Params/env are written just below, over the zeros.
    b.emit(Instruction::LocalGet(childframe_l));
    b.emit(Instruction::I32Const(0));
    b.emit(Instruction::I32Const(size));
    b.emit(Instruction::MemoryFill(0));
    if tpc > 0 {
        let key = (b.module_key.clone(), loc.line, loc.column);
        let type_args = b
            .checker()
            .generic_type_args
            .get(&key)
            .cloned()
            .unwrap_or_default();
        for i in 0..tpc {
            let type_name = type_args.get(i).cloned().unwrap_or_default();
            let (off, len) = b.ctx.strings.borrow_mut().intern(&type_name);
            b.emit(Instruction::LocalGet(childframe_l));
            b.emit(Instruction::I32Const(off as i32));
            b.emit(Instruction::I32Const(len as i32));
            b.emit(Instruction::Call(b.rt().base + RT_ALLOC_STRING));
            b.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }
    }
    // Write each real param: the provided arg, or — for an omitted optional
    // param — its default expression. The sync call path (`compile_call`) does
    // this; the spawn path must too, or an omitted `loader Loader?, default:
    // null` leaves a zero-initialized frame slot and a downstream `loader !=
    // null` guard wrongly succeeds (forking `doLoad` with a null loader →
    // `call_indirect` on garbage). Mirrors `compile_call`'s default fill.
    let (real_param_count, defaults, param_names) = match b.function_by_name.get(callee).copied() {
        Some(p) => {
            let fi = &b.functions()[p as usize];
            (
                (fi.param_count as usize).saturating_sub(tpc),
                fi.param_defaults.clone(),
                fi.param_names.clone(),
            )
        }
        None => (
            args.len(),
            Vec::new(),
            args.iter()
                .enumerate()
                .map(|(i, _)| format!("${}", i))
                .collect(),
        ),
    };
    let labelled_order = if args.iter().any(|a| a.label.is_some()) {
        if param_names.len() != real_param_count {
            return Err(BuildError::UnsupportedExpression(
                "async-spawn-label-param-shape-mismatch",
            ));
        }
        let mut order: Vec<Option<usize>> = vec![None; real_param_count];
        let mut positional_idx = 0usize;
        let mut seen_named = false;
        for (arg_idx, arg) in args.iter().enumerate() {
            if let Some(label) = arg.label {
                seen_named = true;
                let Some(param_idx) = param_names.iter().position(|p| p == label) else {
                    return Err(BuildError::UnsupportedExpression(
                        "async-spawn-unknown-labelled-arg",
                    ));
                };
                if order[param_idx].is_some() {
                    return Err(BuildError::UnsupportedExpression(
                        "async-spawn-duplicate-labelled-arg",
                    ));
                }
                order[param_idx] = Some(arg_idx);
            } else {
                if seen_named {
                    return Err(BuildError::UnsupportedExpression(
                        "async-spawn-positional-after-labelled",
                    ));
                }
                if positional_idx >= real_param_count {
                    return Err(BuildError::UnsupportedExpression(
                        "async-spawn-positional-out-of-range",
                    ));
                }
                if order[positional_idx].is_some() {
                    return Err(BuildError::UnsupportedExpression(
                        "async-spawn-duplicate-positional-arg",
                    ));
                }
                order[positional_idx] = Some(arg_idx);
                positional_idx += 1;
            }
        }
        Some(order)
    } else {
        None
    };
    for i in 0..real_param_count {
        b.emit(Instruction::LocalGet(childframe_l));
        // RC: every param slot OWNS exactly +1 (plan 114 follow-up) —
        // retain a borrowed arg, transfer a fresh/owned one — and the
        // child releases its param slots at completion. Without this the
        // spawner's owned arg temps (a fresh closure / dict / concat
        // passed to an async fn) had no release point and leaked one ref
        // per call: forui's per-render view-builder closures, exactly.
        if let Some(arg_idx) = labelled_order.as_ref().and_then(|order| order[i]) {
            let result = b.compile_expr_result_as(args[arg_idx].value, ValueShape::Boxed)?;
            b.prepare_stack_for_owning_store(result);
        } else if labelled_order.is_none() {
            if let Some(arg) = args.get(i) {
                let result = b.compile_expr_result_as(arg.value, ValueShape::Boxed)?;
                b.prepare_stack_for_owning_store(result);
            } else if let Some(Some(default_expr)) = defaults.get(i + tpc) {
                let result = b.compile_expr_result_as(default_expr, ValueShape::Boxed)?;
                b.prepare_stack_for_owning_store(result);
            } else {
                return Err(BuildError::UnsupportedExpression(
                    "async-spawn-arg-count-mismatch",
                ));
            }
        } else if let Some(Some(default_expr)) = defaults.get(i + tpc) {
            let result = b.compile_expr_result_as(default_expr, ValueShape::Boxed)?;
            b.prepare_stack_for_owning_store(result);
        } else {
            return Err(BuildError::UnsupportedExpression(
                "async-spawn-arg-count-mismatch",
            ));
        }
        b.emit(Instruction::I64Store(mem_off(((tpc + i) as u64) * 8)));
    }
    b.emit(Instruction::I32Const(tidx as i32));
    b.emit(Instruction::LocalGet(childframe_l));
    b.emit(Instruction::Call(layout.spawn));
    b.emit(Instruction::LocalSet(childid_l));
    // Record the child's frame size so it's reclaimed when the task completes.
    b.emit(Instruction::GlobalGet(layout.g_table_base));
    b.emit(Instruction::LocalGet(childid_l));
    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
    b.emit(Instruction::I32Mul);
    b.emit(Instruction::I32Add);
    b.emit(Instruction::I32Const(size));
    b.emit(Instruction::I32Store(mem_off(
        crate::async_engine::O_FRAME_SIZE,
    )));
    Ok(())
}

/// Emit `RT_RELEASE(var_local[name])` for each owned body-binding frame slot at
/// an async completion terminator — the async analogue of sync `pop_scope`
/// (plan 115 Part 1). Each name is loaded from its wasm local (reloaded from the
/// frame at segment entry) and released; RT_RELEASE no-ops on primitives and on
/// the zero a never-written slot holds (frames are zeroed at spawn). The caller
/// must retain a borrowed result/error value BEFORE calling this so it survives
/// the releases (the +1-return convention). RT_RELEASE is stack-neutral
/// (i64)->(), so any value already on the stack is left untouched.
fn emit_async_drops(
    b: &mut Builder,
    names: &[String],
    var_local: &std::collections::HashMap<String, u32>,
    cell_offsets: &[u64],
    frame_ptr_l: u32,
) {
    if names.is_empty() && cell_offsets.is_empty() {
        return;
    }
    for name in names {
        if let Some(&local) = var_local.get(name) {
            b.release_owned_local(local, OwnershipOp::Cleanup);
        }
    }
    // Plan 114: release the frame's co-ownership of each heap CELL (the
    // boxed pointer stored in its slot — read from the frame, which is
    // still live here; `complete` frees it after). RT_RELEASE's CELL
    // branch frees the held value and the block at rc 0; a closure that
    // captured the cell holds its own retained ref, so an escaped
    // closure keeps the cell alive past the task.
    for &off in cell_offsets {
        b.emit(Instruction::LocalGet(frame_ptr_l));
        b.emit(Instruction::I64Load(mem_off(off)));
        b.emit_ownership_event_for_stack(OwnershipOp::Cleanup, OWNERSHIP_SITE_UNKNOWN, 0);
        b.emit(Instruction::Call(b.rt().base + RT_RELEASE));
    }
}

/// Build an async function's resume function: a `br_table` on the current
/// task's resume_state dispatches to each segment. At each segment entry
/// the frame pointer and frame-backed locals are reloaded; a pending
/// await result is read into its binding. Each non-final segment runs its
/// statements then suspends (`sleep` or spawn-child + `await`); the final
/// segment `complete`s the task with the result value.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_resume_fn(
    ctx: &BuildContext,
    fd: &FunctionDeclaration,
    frame: &AsyncFrame,
    fn_table_idx: &std::collections::HashMap<String, u32>,
    frame_sizes: &std::collections::HashMap<String, i32>,
    layout: &crate::async_engine::SchedLayout,
    fns: &AsyncResolve<'_>,
    module_context: Option<&str>,
    file_path: Option<&str>,
    outer: Option<&OuterScopeView>,
) -> Result<(Function, Vec<CaptureBinding>), BuildError> {
    let params: std::collections::HashSet<String> =
        fd.params.iter().map(|p| p.name.clone()).collect();
    // Escaping-name set for the real function — feeds both the CFG's per-
    // iteration loop-body drops and the Builder's inline-statement drops.
    let escaping = fai_compiler::escape_analysis::conservative_escaping(fd);
    let blocks = build_cfg(&fd.body, fns, &params, &escaping)
        .ok_or(BuildError::UnsupportedExpression("async-shape"))?;
    let b_count = blocks.len();

    // Param-less, empty-body view so the Builder doesn't create wasm
    // params or scan a body — we drive the CFG and bindings manually.
    let mut fd_view = fd.clone();
    fd_view.params = Vec::new();
    fd_view.type_params = Vec::new();
    fd_view.body = Vec::new();
    // `outer` is `Some` only for an async *closure* — its body may reference
    // upvalues, captured against the enclosing scope. Named fns pass `None`.
    let mut b = Builder::new(&fd_view, ctx, outer);
    // Module context so cross-module names + peer calls in the body resolve
    // the same way the sync path resolves them.
    if let Some(m) = module_context {
        b.module_context = Some(m.to_string());
    }
    // Per-call-site key source (UFCS / named-param / expression-type lookups);
    // mirror `build_function` so checker-recorded entries round-trip.
    b.module_key = file_path
        .or(module_context)
        .map(String::from)
        .unwrap_or_default();

    let frame_ptr_l = b.alloc_i32_local();
    let childframe_l = b.alloc_i32_local();
    let childid_l = b.alloc_i32_local();
    // Heap address of a closure being spawned/called (Term::AwaitClosure).
    let closure_addr_l = b.alloc_i32_local();
    // Sync-closure dispatch path: saved env_ptr, the inline call result, and a
    // synthesized completed-task id/addr so the result reads back uniformly.
    let saved_env_l = b.alloc_i32_local();
    let sync_result_l = b.alloc_local();
    let synth_id_l = b.alloc_i32_local();
    let synth_addr_l = b.alloc_i32_local();
    // Holds a value-`try`/`catch` body's result across a (non-suspending)
    // `finally` until the task completes.
    let try_result_l = b.alloc_local();
    // Holds a failed child's error read from the current task's error slot.
    let child_err_l = b.alloc_local();
    // Frame vars captured-and-mutated by a nested closure must be *cells*: the
    // closure shares the storage, not a snapshot. The frame slot IS the cell —
    // `cell_addr = frame_ptr + offset` (a stable heap address that survives
    // suspension); reads/writes deref it, and the closure captures it. Such a
    // var's local holds that address (i32), not the value.
    let cell_vars = collect_cell_captured_vars(&fd.body);
    // The Builder was constructed from an empty-body `fd_view`, so it has no
    // cell knowledge of its own. Seed it from the real body so `compile_bindings`
    // treats `var s = …` of a captured var as a cell (and, per the reuse path
    // there, stores into the frame slot we bind below rather than overriding it
    // with a plain local).
    b.cell_captured_vars = cell_vars.clone();
    // Likewise seed the escaping set from the REAL body, not the empty
    // `fd_view`. The unified scope-drop mechanism (`note_droppable`) fires when
    // a non-suspending nested loop/if/case in this function is compiled inline
    // via `compile_stmt`; without the real set it would skip the escape check
    // and over-drop. With it, confined fresh-literals in those non-suspending
    // blocks are freed per scope-exit just as in a sync function. (Bindings in
    // SUSPENDING loop bodies go through the CFG segment path, which doesn't
    // drop yet — sound leak.)
    b.confined_escaping = escaping.clone();
    let mut var_local: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for v in &frame.vars {
        if cell_vars.contains(v) {
            let l = b.alloc_i32_local();
            b.bind_cell(v, l);
            var_local.insert(v.clone(), l);
        } else {
            let l = b.alloc_local();
            b.bind(v, l);
            var_local.insert(v.clone(), l);
        }
    }

    // Async reclamation (plan 115 Part 1): every owned body-binding frame slot
    // is RT_RELEASE'd at every completion terminator — the async analogue of
    // sync `pop_scope`. This turns the UNBOUNDED per-invocation leak (a request
    // handler's parsed body / DB rows / rendered HTML) into a steady-state
    // plateau: the slots are freed exactly when the task finishes.
    //
    // Soundness relies on each released slot owning exactly `+1` at completion:
    //   - let/var bindings get the +1 via transfer (fresh / call-result) or a
    //     retain-on-borrow at bind time (see `compile_async_segment_stmt`).
    //   - await-result bindings receive the child's `+1` result (transfer).
    //   - frames are zeroed at spawn, so a slot not written on the path to a
    //     completion reads 0 (a safe RT_RELEASE no-op) rather than a stale
    //     pointer from a recycled frame block.
    // The result/error value is retained-if-borrowed before the releases at each
    // terminator so it survives (the +1-return convention), exactly like sync
    // `compile_return`.
    //
    // Excluded: cell-captured vars (released through their frame SLOT below,
    // not their addr local — plan 114), catch vars and multi-assignment
    // targets (`a, b = …` plain-overwrites — rc not guaranteed +1).
    // Single-name reassigned vars ARE released: their locals are marked
    // owned below, so binding release-the-old + `compile_assignment`
    // retain-new/release-old keep them at exactly `+1` (plan 116 follow-up).
    // Params and type-params are released too (plan 114 follow-up): every
    // spawn site now stores an OWNED `+1` into each param slot
    // (retain-if-borrowed in `emit_spawn_child` / `Term::AwaitClosure` /
    // `emit_drive_closure`; type-arg strings are interned fresh), so the
    // task releasing them at completion is what closes the
    // owned-argument-to-async-call leak.
    let mut excluded: std::collections::HashSet<String> = cell_vars.clone();
    collect_multi_rebound_names(&fd.body, &mut excluded);
    collect_catch_names(&fd.body, &mut excluded);
    let release_names: Vec<String> = frame
        .vars
        .iter()
        .filter(|v| !excluded.contains(*v) && var_local.contains_key(*v))
        .cloned()
        .collect();
    let release_set: std::collections::HashSet<String> = release_names.iter().cloned().collect();
    // Reassignment of a release-set var must keep the slot at one owned ref:
    // mark the local so `compile_assignment` retains-new/releases-old exactly
    // like a sync owned local (these are completion-released, never
    // scope-dropped, so they don't go through `note_droppable`).
    for name in &release_names {
        if let Some(&l) = var_local.get(name) {
            b.owned_frame_locals.insert(l);
        }
    }
    // Cell vars are released at completion through their frame SLOT (the
    // boxed heap-cell pointer, plan 114), not their addr local — collect
    // the slot offsets for `emit_async_drops`.
    let cell_offsets: Vec<u64> = frame
        .vars
        .iter()
        .filter(|v| cell_vars.contains(*v))
        .map(|v| frame.var_off[v])
        .collect();

    let store_vars = |b: &mut Builder| {
        for v in &frame.vars {
            // Cell slots hold the boxed heap-cell pointer, written once at
            // first entry; the mutable value lives in the cell — nothing to
            // flush.
            if cell_vars.contains(v) {
                continue;
            }
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::LocalGet(var_local[v]));
            b.emit(Instruction::I64Store(mem_off(frame.var_off[v])));
        }
    };

    // Function entry: recover the frame pointer and reload frame-backed
    // locals once per (re)entry. Within an invocation, jumps re-dispatch
    // through the loop without reloading — locals stay live in wasm locals.
    emit_load_current_frame(&mut b, layout, frame_ptr_l);
    // Closure: seed `__env_ptr` from frame[0] so upvalue reads resolve. Done at
    // every (re)entry — a child await re-enters from the top and re-seeds.
    if frame.has_env {
        b.emit(Instruction::LocalGet(frame_ptr_l));
        b.emit(Instruction::I32Load(mem_off(0)));
        b.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));
    }
    for v in &frame.vars {
        if cell_vars.contains(v) {
            // Plan 114: the frame slot holds the NaN-boxed pointer of a
            // HEAP cell, not the cell itself. First entry (slot reads 0 —
            // frames are zeroed at spawn): allocate + tag the cell and
            // store its boxed pointer into the slot. Every entry: unbox
            // the pointer into the addr local. A heap cell survives the
            // frame, so an escaped closure that captured it stays valid
            // after the task completes — which is what lets frames with
            // cells be reclaimed again (the old design leaked the whole
            // frame to keep escaped closures safe).
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::I64Load(mem_off(frame.var_off[v])));
            b.emit(Instruction::I64Eqz);
            b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
            {
                let addr = var_local[v];
                b.emit(Instruction::I32Const(16));
                b.emit(Instruction::Call(b.rt().base + RT_ALLOC));
                b.emit(Instruction::LocalTee(addr));
                b.emit(Instruction::I64Const(crate::runtime::OBJ_TAG_CELL as i64));
                b.emit(Instruction::I64Store(mem0()));
                b.emit(Instruction::LocalGet(addr));
                b.emit(Instruction::I64Const(0));
                b.emit(Instruction::I64Store(mem_off(8)));
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::LocalGet(addr));
                b.emit(Instruction::Call(b.rt().base + RT_MAKE_OBJ));
                b.emit(Instruction::I64Store(mem_off(frame.var_off[v])));
            }
            b.emit(Instruction::End);
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::I64Load(mem_off(frame.var_off[v])));
            b.emit(Instruction::Call(b.rt().base + RT_OBJ_ADDR));
            b.emit(Instruction::LocalSet(var_local[v]));
        } else {
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::I64Load(mem_off(frame.var_off[v])));
            b.emit(Instruction::LocalSet(var_local[v]));
        }
    }

    // loop { block^B { br_table(resume_state) } <block bodies> }
    b.emit(Instruction::Loop(wasm_encoder::BlockType::Empty));
    for _ in 0..b_count {
        b.emit(Instruction::Block(wasm_encoder::BlockType::Empty));
    }
    emit_load_current_rstate(&mut b, layout);
    let targets: Vec<u32> = (0..b_count as u32).collect();
    b.emit(Instruction::BrTable(targets.into(), 0));

    for (k, blk) in blocks.iter().enumerate() {
        b.emit(Instruction::End); // block k region lands here
                                  // br index to reach the enclosing loop from this region.
        let loop_depth = (b_count - 1 - k) as u32;

        // If a child failed, the scheduler recorded the first-completed
        // error in this task's error slot. Route it: into the enclosing
        // `catch` (binding it) if the await was inside a `try`, else fail
        // this task (propagating up the await chain). The slot is reset so a
        // later await in the catch path starts clean.
        let check_child_error =
            |b: &mut Builder, on_error: Option<&(usize, String)>| -> Result<(), BuildError> {
                let cur_addr = |b: &mut Builder| {
                    b.emit(Instruction::GlobalGet(layout.g_table_base));
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                    b.emit(Instruction::I32Mul);
                    b.emit(Instruction::I32Add);
                };
                cur_addr(b);
                b.emit(Instruction::I64Load(mem_off(crate::async_engine::O_ERROR)));
                b.emit(Instruction::LocalSet(child_err_l));
                b.emit(Instruction::LocalGet(child_err_l));
                b.emit(Instruction::I64Const(VAL_VOID));
                b.emit(Instruction::I64Ne);
                b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
                // Reset the error slot.
                cur_addr(b);
                b.emit(Instruction::I64Const(VAL_VOID));
                b.emit(Instruction::I64Store(mem_off(crate::async_engine::O_ERROR)));
                match on_error {
                    Some((catch_blk, err_var)) => {
                        let l = *var_local
                            .get(err_var)
                            .ok_or(BuildError::UnsupportedExpression("async-unknown-catch"))?;
                        b.emit(Instruction::LocalGet(child_err_l));
                        b.emit(Instruction::LocalSet(l));
                        emit_store_current_rstate(b, layout, *catch_blk as i32);
                        // +1: this `br` is inside the error-check `If` block.
                        b.emit(Instruction::Br(loop_depth + 1));
                    }
                    None => {
                        b.emit(Instruction::GlobalGet(layout.g_current));
                        b.emit(Instruction::LocalGet(child_err_l));
                        b.emit(Instruction::Call(layout.fail));
                        b.emit(Instruction::Return);
                    }
                }
                b.emit(Instruction::End);
                Ok(())
            };
        let check_global_error =
            |b: &mut Builder, on_error: Option<&(usize, String)>| -> Result<(), BuildError> {
                b.emit(Instruction::GlobalGet(GLOBAL_ERROR_FLAG));
                b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
                b.emit(Instruction::GlobalGet(GLOBAL_ERROR_VALUE));
                b.emit(Instruction::LocalSet(child_err_l));
                b.emit(Instruction::I32Const(0));
                b.emit(Instruction::GlobalSet(GLOBAL_ERROR_FLAG));
                b.emit(Instruction::I64Const(0));
                b.emit(Instruction::GlobalSet(GLOBAL_ERROR_VALUE));
                match on_error {
                    Some((catch_blk, err_var)) => {
                        let l = *var_local
                            .get(err_var)
                            .ok_or(BuildError::UnsupportedExpression("async-unknown-catch"))?;
                        b.emit(Instruction::LocalGet(child_err_l));
                        b.emit(Instruction::LocalSet(l));
                        emit_store_current_rstate(b, layout, *catch_blk as i32);
                        // +1: this `br` is inside the error-check `If` block.
                        b.emit(Instruction::Br(loop_depth + 1));
                    }
                    None => {
                        b.emit(Instruction::GlobalGet(layout.g_current));
                        b.emit(Instruction::LocalGet(child_err_l));
                        b.emit(Instruction::Call(layout.fail));
                        b.emit(Instruction::Return);
                    }
                }
                b.emit(Instruction::End);
                Ok(())
            };
        let assign_pending = |b: &mut Builder, slot: u64, name: &str| -> Result<(), BuildError> {
            let l = *var_local
                .get(name)
                .ok_or(BuildError::UnsupportedExpression("async-unknown-bind"))?;
            if cell_vars.contains(name) {
                // Store the awaited result through the heap cell with
                // value-RC (plan 114). The child's +1 result transfers.
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::I32Load(mem_off(frame.pending_off + slot * 4)));
                b.emit(Instruction::Call(layout.task_result));
                b.emit_cell_store(l, ExprResult::boxed(true));
            } else {
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::I32Load(mem_off(frame.pending_off + slot * 4)));
                b.emit(Instruction::Call(layout.task_result));
                b.assign_to_async_frame_slot(
                    l,
                    ExprResult::boxed(true),
                    release_set.contains(name),
                );
            }
            Ok(())
        };
        // Recycle child task `slot`'s record onto the free list. The waiter only
        // resumes once its children have completed (join count hit 0), so the
        // slot is done and its result already consumed here. IDEMPOTENT: only a
        // slot still in a terminal (COMPLETE/FAILED) state is freed, and freeing
        // marks it ST_FREED — so a slot is never pushed onto `g_free_head` twice
        // (a double-free would hand the same slot to two live tasks via `spawn`,
        // e.g. a parent and its own child → self-await → poll re-readies it
        // forever). A slot already freed (or live/reused, status READY/RUNNING/
        // WAITING) is skipped.
        let free_pending = |b: &mut Builder, slot: u64| {
            let pend = b.alloc_i32_local();
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::I32Load(mem_off(frame.pending_off + slot * 4)));
            b.emit(Instruction::LocalSet(pend));
            // status = task[pend].status
            let st = b.alloc_i32_local();
            b.emit(Instruction::GlobalGet(layout.g_table_base));
            b.emit(Instruction::LocalGet(pend));
            b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            b.emit(Instruction::I32Mul);
            b.emit(Instruction::I32Add);
            b.emit(Instruction::I32Load(mem_off(crate::async_engine::O_STATUS)));
            b.emit(Instruction::LocalSet(st));
            // if status == COMPLETE || status == FAILED:
            b.emit(Instruction::LocalGet(st));
            b.emit(Instruction::I32Const(crate::async_engine::ST_COMPLETE));
            b.emit(Instruction::I32GeS);
            b.emit(Instruction::LocalGet(st));
            b.emit(Instruction::I32Const(crate::async_engine::ST_FAILED));
            b.emit(Instruction::I32LeS);
            b.emit(Instruction::I32And);
            b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
            // Drop the scheduler record's stored result. Any consumer already
            // received its own retained +1 through `task_result`.
            b.emit(Instruction::GlobalGet(layout.g_table_base));
            b.emit(Instruction::LocalGet(pend));
            b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            b.emit(Instruction::I32Mul);
            b.emit(Instruction::I32Add);
            b.emit(Instruction::I64Load(mem_off(crate::async_engine::O_RESULT)));
            b.emit(Instruction::Call(b.rt().base + RT_RELEASE));
            // task[pend].next = g_free_head; g_free_head = pend
            b.emit(Instruction::GlobalGet(layout.g_table_base));
            b.emit(Instruction::LocalGet(pend));
            b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            b.emit(Instruction::I32Mul);
            b.emit(Instruction::I32Add);
            b.emit(Instruction::GlobalGet(layout.g_free_head));
            b.emit(Instruction::I32Store(mem_off(crate::async_engine::O_NEXT)));
            b.emit(Instruction::LocalGet(pend));
            b.emit(Instruction::GlobalSet(layout.g_free_head));
            // task[pend].status = ST_FREED
            b.emit(Instruction::GlobalGet(layout.g_table_base));
            b.emit(Instruction::LocalGet(pend));
            b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            b.emit(Instruction::I32Mul);
            b.emit(Instruction::I32Add);
            b.emit(Instruction::I32Const(crate::async_engine::ST_FREED));
            b.emit(Instruction::I32Store(mem_off(
                crate::async_engine::O_STATUS,
            )));
            b.emit(Instruction::End);
        };
        if let Incoming::Awaited { binds, on_error } = &blk.incoming {
            // A failed child routes its error (to catch, or fails this task).
            // Otherwise bind the named results, then recycle each child slot —
            // including discarded ones (`binds[i] == None`, e.g. a `children()`
            // statement), which would otherwise leak a slot per render.
            check_child_error(&mut b, on_error.as_ref())?;
            for (slot, bind) in binds.iter().enumerate() {
                if let Some(name) = bind {
                    assign_pending(&mut b, slot as u64, name)?;
                } else {
                    // Discarded result (e.g. a `children()` statement). The child
                    // completed with an owned `+1` result; with no binding to take
                    // ownership it would leak, so release it here. RT_RELEASE
                    // no-ops on a primitive / void result.
                    b.emit(Instruction::LocalGet(frame_ptr_l));
                    b.emit(Instruction::I32Load(mem_off(
                        frame.pending_off + (slot as u64) * 4,
                    )));
                    b.emit(Instruction::Call(layout.task_result));
                    b.emit(Instruction::Call(b.rt().base + RT_RELEASE));
                }
                free_pending(&mut b, slot as u64);
            }
        }
        if let Incoming::AwaitedRemote { bind, on_error } = &blk.incoming {
            // The `remoteCall` finished; read its result for the current task.
            b.emit(Instruction::GlobalGet(layout.g_current));
            b.emit_import_call(crate::runtime::IMPORT_REMOTE_RESULT);
            let remote_result_l = b.alloc_local();
            b.emit(Instruction::LocalSet(remote_result_l));
            check_global_error(&mut b, on_error.as_ref())?;
            if let Some(name) = bind {
                let l = *var_local
                    .get(name)
                    .ok_or(BuildError::UnsupportedExpression(
                        "async-unknown-remote-bind",
                    ))?;
                if cell_vars.contains(name) {
                    // Value-RC store through the heap cell (plan 114); the
                    // host-built RPC result transfers.
                    b.emit(Instruction::LocalGet(remote_result_l));
                    b.emit_cell_store(l, ExprResult::boxed(true));
                } else {
                    b.emit(Instruction::LocalGet(remote_result_l));
                    b.assign_to_async_frame_slot(
                        l,
                        ExprResult::boxed(true),
                        release_set.contains(name),
                    );
                }
            } else {
                // Result discarded — still consume it to free the host's slot.
                b.emit(Instruction::LocalGet(remote_result_l));
                b.emit(Instruction::Drop);
            }
        }
        if let Incoming::AwaitedFfi { bind } = &blk.incoming {
            // The offloaded extern call finished; read its result for this task.
            b.emit(Instruction::GlobalGet(layout.g_current));
            b.emit_import_call(crate::runtime::IMPORT_FFI_RESULT);
            let ffi_result_l = b.alloc_local();
            b.emit(Instruction::LocalSet(ffi_result_l));
            if let Some(name) = bind {
                let l = *var_local
                    .get(name)
                    .ok_or(BuildError::UnsupportedExpression("async-unknown-ffi-bind"))?;
                if cell_vars.contains(name) {
                    b.emit(Instruction::LocalGet(ffi_result_l));
                    b.emit_cell_store(l, ExprResult::boxed(true));
                } else {
                    b.emit(Instruction::LocalGet(ffi_result_l));
                    b.assign_to_async_frame_slot(
                        l,
                        ExprResult::boxed(true),
                        release_set.contains(name),
                    );
                }
            } else {
                b.emit(Instruction::LocalGet(ffi_result_l));
                b.emit(Instruction::Drop);
            }
        }
        if let Incoming::AwaitedHostOp { bind, on_error } = &blk.incoming {
            // The offloaded generic host operation finished; read its result for
            // this task. Host ops share the global error channel used by remote
            // calls so async try/catch can catch operation-specific failures.
            b.emit(Instruction::GlobalGet(layout.g_current));
            b.emit_import_call(crate::runtime::IMPORT_HOST_OP_RESULT);
            let host_result_l = b.alloc_local();
            b.emit(Instruction::LocalSet(host_result_l));
            check_global_error(&mut b, on_error.as_ref())?;
            if let Some(name) = bind {
                let l = *var_local
                    .get(name)
                    .ok_or(BuildError::UnsupportedExpression(
                        "async-unknown-host-op-bind",
                    ))?;
                if cell_vars.contains(name) {
                    b.emit(Instruction::LocalGet(host_result_l));
                    b.emit_cell_store(l, ExprResult::boxed(true));
                } else {
                    b.emit(Instruction::LocalGet(host_result_l));
                    b.assign_to_async_frame_slot(
                        l,
                        ExprResult::boxed(true),
                        release_set.contains(name),
                    );
                }
            } else {
                b.emit(Instruction::LocalGet(host_result_l));
                b.emit(Instruction::Drop);
            }
        }

        for stmt in &blk.stmts {
            if let Statement::NowaitStatement(nw) = stmt {
                let (callee, args, loc) = user_callee(&nw.expression, fns)
                    .ok_or(BuildError::UnsupportedExpression("nowait-non-call"))?;
                emit_spawn_child(
                    &mut b,
                    &callee,
                    &args,
                    loc,
                    frame_sizes,
                    fn_table_idx,
                    layout,
                    childframe_l,
                    childid_l,
                )?;
                continue;
            }
            let catch = match &blk.on_error {
                Some((catch_blk, err_var)) => {
                    let l = *var_local
                        .get(err_var)
                        .ok_or(BuildError::UnsupportedExpression("async-unknown-catch"))?;
                    Some((*catch_blk, l))
                }
                None => None,
            };
            b.async_error_ctx = Some(AsyncErrorContext {
                layout: *layout,
                loop_depth,
                catch,
            });
            compile_async_segment_stmt(&mut b, stmt, &var_local, &release_set)?;
            b.async_error_ctx = None;
        }

        // (R1 clean slate, plan 113: async loop-body auto-drops removed — RC
        // reclaims uniformly.)

        match &blk.term {
            Term::Unset => return Err(BuildError::UnsupportedExpression("async-unset-block")),
            Term::Goto(t) => {
                emit_store_current_rstate(&mut b, layout, *t as i32);
                b.emit(Instruction::Br(loop_depth));
            }
            Term::Cond {
                cond,
                then_blk,
                else_blk,
            } => {
                // resume_state = cond ? then_blk : else_blk; re-dispatch.
                b.emit(Instruction::GlobalGet(layout.g_table_base));
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                b.emit(Instruction::I32Mul);
                b.emit(Instruction::I32Add);
                b.compile_expr_as(cond, ValueShape::RawBool)?;
                b.emit(Instruction::If(wasm_encoder::BlockType::Result(
                    ValType::I32,
                )));
                b.emit(Instruction::I32Const(*then_blk as i32));
                b.emit(Instruction::Else);
                b.emit(Instruction::I32Const(*else_blk as i32));
                b.emit(Instruction::End);
                b.emit(Instruction::I32Store(mem_off(
                    crate::async_engine::O_RSTATE,
                )));
                b.emit(Instruction::Br(loop_depth));
            }
            Term::Sleep { ms, next } => {
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.compile_expr(ms)?;
                b.emit(Instruction::Call(b.rt().base + RT_AS_NUMBER));
                b.emit(Instruction::Call(layout.sleep));
                b.emit(Instruction::Return);
            }
            Term::AwaitRemote { args, next } => {
                // Park the current task on the in-flight request (no timer): the
                // host wakes it via `__fai_resume_task` when the response lands.
                emit_park_current_task(&mut b, layout);
                // remote_begin(g_current, url*,len, fn*,len, args*,len, hash*,len)
                b.emit(Instruction::GlobalGet(layout.g_current));
                let mut stashes: Vec<u32> = Vec::new();
                for a in args {
                    if let Some(t) = b.emit_string_arg_stashing(a)? {
                        stashes.push(t);
                    }
                }
                b.emit_import_call(crate::runtime::IMPORT_REMOTE_BEGIN);
                for t in stashes {
                    b.release_stash(Some(t));
                }
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
            }
            Term::AwaitFfi {
                ext_idx,
                args,
                next,
            } => {
                // Park the task (status WAITING, O_WAKE = -1) and offload the
                // blocking extern call to the boundary; the driver loop resumes
                // it via `__fai_resume_task` when the worker finishes.
                emit_park_current_task(&mut b, layout);
                // Build the args buffer (one NaN-boxed i64 per arg), as a sync
                // extern call would; `ffi_begin` copies the args out before the
                // task parks, so the buffer can be freed right after.
                let arg_count = args.len() as i32;
                let (buf, byte_len, owned_arg_locals) = emit_boxed_arg_buffer(&mut b, args)?;
                // ffi_begin(g_current, ext_idx, arg_count, args_buf)
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::I32Const(*ext_idx as i32));
                b.emit(Instruction::I32Const(arg_count));
                b.emit(Instruction::LocalGet(buf));
                b.emit_import_call(crate::runtime::IMPORT_FFI_BEGIN);
                emit_release_owned_arg_locals(&mut b, &owned_arg_locals);
                // rt_free(ptr, size) — same size passed to rt_alloc above.
                b.emit(Instruction::LocalGet(buf));
                b.emit(Instruction::I32Const(byte_len));
                b.emit(Instruction::Call(b.rt().base + crate::runtime::RT_FREE));
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
            }
            Term::AwaitHostOp {
                op_kind,
                args,
                loc: _,
                next,
            } => {
                // Generic async host operation: park this task, submit owned
                // copied arguments to the host, then resume at `next` when the
                // boundary completion is ready.
                emit_park_current_task(&mut b, layout);
                let arg_count = args.len() as i32;
                let (buf, byte_len, owned_arg_locals) = emit_boxed_arg_buffer(&mut b, args)?;
                // host_op_begin(g_current, op_kind, arg_count, args_buf)
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::I32Const(*op_kind));
                b.emit(Instruction::I32Const(arg_count));
                b.emit(Instruction::LocalGet(buf));
                b.emit_import_call(crate::runtime::IMPORT_HOST_OP_BEGIN);
                emit_release_owned_arg_locals(&mut b, &owned_arg_locals);
                b.emit(Instruction::LocalGet(buf));
                b.emit(Instruction::I32Const(byte_len));
                b.emit(Instruction::Call(b.rt().base + crate::runtime::RT_FREE));
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
            }
            Term::CompleteRemote { on_error } => {
                // complete(g_current, remote_result(g_current)) — the RPC result
                // is this (stub) task's return value. The result is host-provided
                // (not a frame slot), so releasing the body bindings first can't
                // touch it.
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit_import_call(crate::runtime::IMPORT_REMOTE_RESULT);
                let remote_result_l = b.alloc_local();
                b.emit(Instruction::LocalSet(remote_result_l));
                check_global_error(&mut b, on_error.as_ref())?;
                emit_async_drops(
                    &mut b,
                    &release_names,
                    &var_local,
                    &cell_offsets,
                    frame_ptr_l,
                );
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(remote_result_l));
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
            Term::CompleteHostOp { on_error } => {
                // complete(g_current, host_op_result(g_current)) — the generic
                // host-op result is this task's return value.
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit_import_call(crate::runtime::IMPORT_HOST_OP_RESULT);
                let host_result_l = b.alloc_local();
                b.emit(Instruction::LocalSet(host_result_l));
                check_global_error(&mut b, on_error.as_ref())?;
                emit_async_drops(
                    &mut b,
                    &release_names,
                    &var_local,
                    &cell_offsets,
                    frame_ptr_l,
                );
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(host_result_l));
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
            Term::Await {
                callee,
                args,
                loc,
                next,
            } => {
                emit_spawn_child(
                    &mut b,
                    callee,
                    args,
                    loc,
                    frame_sizes,
                    fn_table_idx,
                    layout,
                    childframe_l,
                    childid_l,
                )?;
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::I32Store(mem_off(frame.pending_off)));
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::Call(layout.await_fn));
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
            }
            Term::AwaitClosure {
                closure,
                args,
                next,
            } => {
                // Evaluate the closure value → heap address.
                b.compile_expr_as(closure, ValueShape::Boxed)?;
                b.emit(Instruction::Call(b.rt().base + RT_OBJ_ADDR));
                b.emit(Instruction::LocalSet(closure_addr_l));
                // Runtime dispatch on the header's frame_size (offset 12):
                //   0  → sync closure (a `FaiFunc`) — call inline, no suspend.
                //   >0 → async closure (resume fn) — spawn as a task + await.
                // Both leave a child-task id in `pending`, so the next segment
                // reads the result uniformly via `task_result`.
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(12)));
                b.emit(Instruction::I32Eqz);
                b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
                // ── sync closure: inline call_indirect(FaiFunc(N)) ──
                let arity = args.len() as u16;
                let fai_ty = *b
                    .ctx
                    .fai_func_type_indices
                    .get(&arity)
                    .ok_or(BuildError::UnsupportedExpression("async-closure-arity"))?;
                b.emit(Instruction::GlobalGet(GLOBAL_ENV_PTR));
                b.emit(Instruction::LocalSet(saved_env_l));
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Const(16));
                b.emit(Instruction::I32Add);
                b.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));
                for arg in args.iter() {
                    b.compile_expr_as(arg, ValueShape::Boxed)?;
                }
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(4)));
                b.emit(Instruction::CallIndirect {
                    type_index: fai_ty,
                    table_index: 0,
                });
                b.emit(Instruction::LocalSet(sync_result_l));
                b.emit(Instruction::LocalGet(saved_env_l));
                b.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));
                // Synthesize a completed task holding the result so the next
                // segment can read it through `task_result` like an async child.
                // Reuse a free slot if available, else bump `g_count` — bumping
                // unconditionally would grow the table by one per *sync* closure
                // call, and the render tree is mostly sync closures, so the table
                // would creep up every render despite `free_pending` recycling.
                b.emit(Instruction::GlobalGet(layout.g_free_head));
                b.emit(Instruction::LocalTee(synth_id_l));
                b.emit(Instruction::I32Const(-1));
                b.emit(Instruction::I32Ne);
                b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
                // pop: g_free_head = freed[synth_id].next
                b.emit(Instruction::GlobalGet(layout.g_table_base));
                b.emit(Instruction::LocalGet(synth_id_l));
                b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                b.emit(Instruction::I32Mul);
                b.emit(Instruction::I32Add);
                b.emit(Instruction::I32Load(mem_off(crate::async_engine::O_NEXT)));
                b.emit(Instruction::GlobalSet(layout.g_free_head));
                b.emit(Instruction::Else);
                b.emit(Instruction::GlobalGet(layout.g_count));
                b.emit(Instruction::LocalSet(synth_id_l));
                b.emit(Instruction::GlobalGet(layout.g_count));
                b.emit(Instruction::I32Const(1));
                b.emit(Instruction::I32Add);
                b.emit(Instruction::GlobalSet(layout.g_count));
                b.emit(Instruction::End);
                b.emit(Instruction::GlobalGet(layout.g_table_base));
                b.emit(Instruction::LocalGet(synth_id_l));
                b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                b.emit(Instruction::I32Mul);
                b.emit(Instruction::I32Add);
                b.emit(Instruction::LocalSet(synth_addr_l));
                b.emit(Instruction::LocalGet(synth_addr_l));
                b.emit(Instruction::I32Const(crate::async_engine::ST_COMPLETE));
                b.emit(Instruction::I32Store(mem_off(
                    crate::async_engine::O_STATUS,
                )));
                b.emit(Instruction::LocalGet(synth_addr_l));
                b.emit(Instruction::LocalGet(sync_result_l));
                b.emit(Instruction::I64Store(mem_off(
                    crate::async_engine::O_RESULT,
                )));
                b.emit(Instruction::LocalGet(synth_addr_l));
                b.emit(Instruction::I64Const(VAL_VOID));
                b.emit(Instruction::I64Store(mem_off(crate::async_engine::O_ERROR)));
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::LocalGet(synth_id_l));
                b.emit(Instruction::I32Store(mem_off(frame.pending_off)));
                // Re-dispatch to `next` without suspending (locals stay live).
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Br(loop_depth + 1));
                b.emit(Instruction::Else);
                // ── async closure: spawn via header + await + suspend ──
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(12)));
                b.emit(Instruction::Call(layout.alloc));
                b.emit(Instruction::LocalSet(childframe_l));
                // Zero the fresh frame (plan 115) — see `emit_spawn_child`. Size
                // is the closure's frame_size (header @ +12). env/args overwrite
                // the leading zeros below.
                b.emit(Instruction::LocalGet(childframe_l));
                b.emit(Instruction::I32Const(0));
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(12)));
                b.emit(Instruction::MemoryFill(0));
                b.emit(Instruction::LocalGet(childframe_l));
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Const(16));
                b.emit(Instruction::I32Add);
                b.emit(Instruction::I32Store(mem_off(0)));
                for (j, arg) in args.iter().enumerate() {
                    b.emit(Instruction::LocalGet(childframe_l));
                    // Param slots own +1 (see `emit_spawn_child`) — retain
                    // a borrowed arg; the closure task releases its param
                    // slots at completion.
                    let result = b.compile_expr_result_as(arg, ValueShape::Boxed)?;
                    b.prepare_stack_for_owning_store(result);
                    b.emit(Instruction::I64Store(mem_off(8 + (j as u64) * 8)));
                }
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(4)));
                b.emit(Instruction::LocalGet(childframe_l));
                b.emit(Instruction::Call(layout.spawn));
                b.emit(Instruction::LocalSet(childid_l));
                // Record the spawned closure frame's size (closure header @ +12)
                // so the task's completion reclaims it.
                b.emit(Instruction::GlobalGet(layout.g_table_base));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                b.emit(Instruction::I32Mul);
                b.emit(Instruction::I32Add);
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(12)));
                b.emit(Instruction::I32Store(mem_off(
                    crate::async_engine::O_FRAME_SIZE,
                )));
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::I32Store(mem_off(frame.pending_off)));
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::Call(layout.await_fn));
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
                b.emit(Instruction::End);
            }
            Term::All { children, next } => {
                // Spawn each child (id → pending slot k) and `await` each so
                // the join count becomes N; resume only when all complete.
                for (j, (callee, args, loc)) in children.iter().enumerate() {
                    emit_spawn_child(
                        &mut b,
                        callee,
                        args,
                        loc,
                        frame_sizes,
                        fn_table_idx,
                        layout,
                        childframe_l,
                        childid_l,
                    )?;
                    b.emit(Instruction::LocalGet(frame_ptr_l));
                    b.emit(Instruction::LocalGet(childid_l));
                    b.emit(Instruction::I32Store(mem_off(
                        frame.pending_off + (j as u64) * 4,
                    )));
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit(Instruction::LocalGet(childid_l));
                    b.emit(Instruction::Call(layout.await_fn));
                }
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
            }
            Term::Complete(expr) => {
                // +1-return convention: the result escapes to the awaiter, which
                // now RELEASES its copy at its own completion, so EVERY completion
                // must hand back an owned `+1`. Retain a borrowed result (it may
                // read a binding); a fresh value / owned call result already is
                // `+1`. Then stash it, release the owned frame bindings, and
                // complete with it: if the result IS a released binding the retain
                // holds it across that release; if it's a FIELD of one, RC
                // deep-free decrements the field but our `+1` keeps it alive.
                b.compile_expr_as(expr, ValueShape::Boxed)?;
                if !b.expr_transfers_ownership(expr) {
                    b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
                }
                let saved = b.alloc_local();
                b.emit(Instruction::LocalSet(saved));
                emit_async_drops(
                    &mut b,
                    &release_names,
                    &var_local,
                    &cell_offsets,
                    frame_ptr_l,
                );
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(saved));
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
            Term::CompleteVoid => {
                // Void is a primitive — no result to retain.
                emit_async_drops(
                    &mut b,
                    &release_names,
                    &var_local,
                    &cell_offsets,
                    frame_ptr_l,
                );
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::I64Const(VAL_VOID));
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
            Term::CompletePending => {
                // A tail/return await: propagate the child's error if it
                // failed, else complete with its result. The result is the
                // child's (read below from its task record, not a frame slot),
                // so releasing the body bindings first can't touch it.
                check_child_error(&mut b, None)?;
                emit_async_drops(
                    &mut b,
                    &release_names,
                    &var_local,
                    &cell_offsets,
                    frame_ptr_l,
                );
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::I32Load(mem_off(frame.pending_off)));
                b.emit(Instruction::Call(layout.task_result));
                // Recycle the awaited child's slot (its result is now consumed)
                // BEFORE `complete` — `complete` now `rt_free`s this task's frame
                // (Phase 4 frame reclaim), and `free_pending` reads the frame's
                // pending slot, so it must run while the frame is still live.
                // `free_pending` is stack-neutral, so [g_current, result] for
                // `complete` is preserved.
                free_pending(&mut b, 0);
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
            Term::ThrowTo {
                value,
                catch_blk,
                err_var,
            } => {
                // Bind the thrown value to the catch name, then jump to the
                // catch handler (within this invocation — no reload needed).
                // Non-dict values are wrapped into `{message: toString(v)}`
                // first so `e.message` in the handler is always a valid
                // read (the wrap allocates fresh, so the catch var's
                // borrowed convention leaks the wrapper — safe, and only
                // on the bare-value throw path).
                b.compile_expr_as(value, ValueShape::Boxed)?;
                let l = *var_local
                    .get(err_var)
                    .ok_or(BuildError::UnsupportedExpression("async-unknown-catch"))?;
                b.emit(Instruction::LocalSet(l));
                let owned = b.expr_transfers_ownership(value);
                b.emit_wrap_bare_throw(l, owned);
                emit_store_current_rstate(&mut b, layout, *catch_blk as i32);
                b.emit(Instruction::Br(loop_depth));
            }
            Term::Fail(value) => {
                // `fail` is a completion: the error escapes to the awaiter's
                // `catch` (read from this task's O_ERROR), so retain-if-borrowed
                // before releasing the owned frame bindings — same +1 convention
                // as a normal `complete`. Bare (non-dict) values are wrapped
                // into `{message: toString(v)}` so the awaiter's `e.message`
                // is always a valid read.
                if release_names.is_empty() {
                    b.compile_expr_as(value, ValueShape::Boxed)?;
                    let saved = b.alloc_local();
                    b.emit(Instruction::LocalSet(saved));
                    let owned = b.expr_transfers_ownership(value);
                    b.emit_wrap_bare_throw(saved, owned);
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit(Instruction::LocalGet(saved));
                    b.emit(Instruction::Call(layout.fail));
                } else {
                    b.compile_expr_as(value, ValueShape::Boxed)?;
                    if !b.expr_transfers_ownership(value) {
                        b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
                    }
                    let saved = b.alloc_local();
                    b.emit(Instruction::LocalSet(saved));
                    b.emit_wrap_bare_throw(saved, true);
                    emit_async_drops(
                        &mut b,
                        &release_names,
                        &var_local,
                        &cell_offsets,
                        frame_ptr_l,
                    );
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit(Instruction::LocalGet(saved));
                    b.emit(Instruction::Call(layout.fail));
                }
                b.emit(Instruction::Return);
            }
            Term::StoreResultGoto { value, next } => {
                // Store the try/catch body's value into the try-result local,
                // then jump to the finally block. The value is held across the
                // finally and escapes via `CompleteResult` to the awaiter (which
                // releases its copy), so retain it if borrowed — the +1-return
                // convention, and it also keeps the value alive across the
                // frame-binding releases the finally / `CompleteResult` emit.
                b.compile_expr_as(value, ValueShape::Boxed)?;
                if !b.expr_transfers_ownership(value) {
                    b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
                }
                b.emit(Instruction::LocalSet(try_result_l));
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Br(loop_depth));
            }
            Term::CompleteResult => {
                // try_result_l already holds a `+1` ref (retained at
                // StoreResultGoto when there are bindings to release).
                emit_async_drops(
                    &mut b,
                    &release_names,
                    &var_local,
                    &cell_offsets,
                    frame_ptr_l,
                );
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(try_result_l));
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
        }
    }
    b.emit(Instruction::End); // close the dispatch loop
    b.emit(Instruction::Unreachable);
    // Upvalues captured during body compilation (closures only; empty for
    // named fns). The creation site uses them to build the env block.
    let upvalues = std::mem::take(&mut b.upvalues);
    Ok((b.finish(), upvalues))
}

/// A-normalize the async calls in a function body: hoist every async call that
/// appears as a *proper subexpression* into a preceding `let __anf_await_N =
/// <call>` temp, so the CFG's await lowering (which only recognizes an async
/// call as the whole value of a `let`/`return`/expr-stmt) can handle shapes like
/// `print(component())` or `f(asyncG())`. Sync calls and async-free expressions
/// are left untouched (no churn on existing code). Positions where hoisting
/// would change evaluation semantics — `&&`/`||` right operands, `while`
/// conditions, `elsif` chains — are deliberately not rewritten; an async call
/// there stays in place and the CFG falls back exactly as before.
fn anf_async_body(body: &[Statement], r: &AsyncResolve<'_>, counter: &mut usize) -> Vec<Statement> {
    let mut out = Vec::with_capacity(body.len());
    for stmt in body {
        anf_async_stmt(stmt, r, counter, &mut out);
    }
    out
}

fn anf_async_stmt(
    stmt: &Statement,
    r: &AsyncResolve<'_>,
    counter: &mut usize,
    out: &mut Vec<Statement>,
) {
    use fai_compiler::ast;
    // Atomize a value position the CFG can't take an async call in (assignment
    // value, `if` condition, `for` items): hoist even a top-level async call.
    let atomize = |expr: &Expression, counter: &mut usize, out: &mut Vec<Statement>| {
        if expr_has_user_call(expr, r) {
            anf_atom(expr, r, counter, out)
        } else {
            expr.clone()
        }
    };
    match stmt {
        Statement::LetStatement(ls) if ls.bindings.len() == 1 => {
            let value = anf_nested(&ls.value, r, counter, out);
            out.push(Statement::LetStatement(ast::LetStatement {
                bindings: ls.bindings.clone(),
                value,
                is_private: ls.is_private,
                is_shared: ls.is_shared,
                location: ls.location.clone(),
            }));
        }
        Statement::VarStatement(vs) if vs.bindings.len() == 1 => {
            let value = anf_nested(&vs.value, r, counter, out);
            out.push(Statement::VarStatement(ast::VarStatement {
                bindings: vs.bindings.clone(),
                value,
                is_private: vs.is_private,
                is_shared: vs.is_shared,
                location: vs.location.clone(),
            }));
        }
        Statement::ReturnStatement(rs) => {
            let value = rs.value.as_ref().map(|v| anf_nested(v, r, counter, out));
            out.push(Statement::ReturnStatement(ast::ReturnStatement {
                value,
                location: rs.location.clone(),
            }));
        }
        Statement::ExpressionStatement(es) => {
            let expression = anf_nested(&es.expression, r, counter, out);
            out.push(Statement::ExpressionStatement(ast::ExpressionStatement {
                expression,
                location: es.location.clone(),
            }));
        }
        Statement::AssignmentStatement(a) => {
            let value = atomize(&a.value, counter, out);
            out.push(Statement::AssignmentStatement(ast::AssignmentStatement {
                target: a.target.clone(),
                value,
                location: a.location.clone(),
            }));
        }
        Statement::IfStatement(is) => {
            // Desugar an else-if chain into nested single-branch ifs (the CFG
            // only lowers single-branch ifs), A-normalizing each piece. Each
            // branch condition is hoisted into the scope where it is actually
            // evaluated: branch 0's into `out` (before the if), branch k>0's
            // into the preceding `else` body (only reached if earlier
            // conditions were false), preserving short-circuit semantics.
            anf_if_chain(
                &is.branches,
                is.else_branch.as_deref(),
                &is.location,
                r,
                counter,
                out,
            );
        }
        Statement::WhileStatement(ws) => {
            // Condition is re-evaluated each iteration — must NOT hoist it.
            let body = anf_async_body(&ws.body, r, counter);
            out.push(Statement::WhileStatement(ast::WhileStatement {
                condition: ws.condition.clone(),
                body,
                location: ws.location.clone(),
            }));
        }
        Statement::ForStatement(fs)
            if (stmts_have_suspension(&fs.body, r) || stmts_have_return(&fs.body))
                && !stmts_have_loop_control(&fs.body) =>
        {
            // A `for` loop whose body suspends or returns can't be compiled inline:
            // a suspension must yield mid-iteration, and a source-level `return`
            // must lower to scheduler `complete()` rather than a raw wasm return
            // from the resume function. Desugar it into an index-driven `while`
            // loop, which the engine already lowers through the async CFG:
            //
            //   for <item> in <start>..<end>:
            //       var __for_idx = <start>
            //       let __for_end = <end>
            //       while __for_idx < __for_end do
            //           let <item> = __for_idx
            //           <body>
            //           __for_idx = __for_idx + 1
            //       end
            //
            //   for <item> in <items>:
            //       let  __for_coll = <items>
            //       var  __for_idx  = 0
            //       while __for_idx < length(__for_coll) do
            //           let <item> = __for_coll[__for_idx]
            //           <body>
            //           __for_idx = __for_idx + 1
            //       end
            //
            // The loop index and collection live in the frame, so they survive a
            // suspension inside the body. Range expressions are not first-class
            // values in direct wasm, so they need the counter form instead of a
            // synthetic `let __for_coll = start..end`. Plain fall-through `for`
            // loops keep the fast inline path below.
            let loc = fs.location.clone();
            let idx_name = format!("__for_idx_{}", *counter);
            *counter += 1;
            let ident = |name: &str| {
                Expression::IdentifierExpression(ast::IdentifierExpression {
                    name: name.to_string(),
                    location: loc.clone(),
                })
            };
            let int_lit = |n: f64| {
                Expression::NumberExpression(ast::NumberExpression {
                    value: n,
                    is_float: false,
                    location: loc.clone(),
                })
            };
            let mut wbody: Vec<Statement> = Vec::new();
            let condition = if let Expression::RangeExpression(range) = &fs.items {
                let end_name = format!("__for_end_{}", *counter);
                *counter += 1;
                let start = atomize(&range.start, counter, out);
                let end = atomize(&range.end, counter, out);
                out.push(Statement::VarStatement(ast::VarStatement {
                    bindings: vec![ast::BindingDeclaration {
                        name: idx_name.clone(),
                        type_name: None,
                    }],
                    value: start,
                    is_private: None,
                    is_shared: None,
                    location: loc.clone(),
                }));
                out.push(Statement::LetStatement(ast::LetStatement {
                    bindings: vec![ast::BindingDeclaration {
                        name: end_name.clone(),
                        type_name: None,
                    }],
                    value: end,
                    is_private: None,
                    is_shared: None,
                    location: loc.clone(),
                }));
                wbody.push(Statement::LetStatement(ast::LetStatement {
                    bindings: vec![ast::BindingDeclaration {
                        name: fs.item_name.clone(),
                        type_name: None,
                    }],
                    value: ident(&idx_name),
                    is_private: None,
                    is_shared: None,
                    location: loc.clone(),
                }));
                Expression::BinaryExpression(ast::BinaryExpression {
                    left: Box::new(ident(&idx_name)),
                    operator: if range.inclusive { "<=" } else { "<" }.to_string(),
                    right: Box::new(ident(&end_name)),
                    location: loc.clone(),
                })
            } else {
                let coll = atomize(&fs.items, counter, out);
                let coll_name = format!("__for_coll_{}", *counter);
                *counter += 1;
                out.push(Statement::LetStatement(ast::LetStatement {
                    bindings: vec![ast::BindingDeclaration {
                        name: coll_name.clone(),
                        type_name: None,
                    }],
                    value: coll,
                    is_private: None,
                    is_shared: None,
                    location: loc.clone(),
                }));
                out.push(Statement::VarStatement(ast::VarStatement {
                    bindings: vec![ast::BindingDeclaration {
                        name: idx_name.clone(),
                        type_name: None,
                    }],
                    value: int_lit(0.0),
                    is_private: None,
                    is_shared: None,
                    location: loc.clone(),
                }));
                wbody.push(Statement::LetStatement(ast::LetStatement {
                    bindings: vec![ast::BindingDeclaration {
                        name: fs.item_name.clone(),
                        type_name: None,
                    }],
                    value: Expression::IndexExpression(ast::IndexExpression {
                        object: Box::new(ident(&coll_name)),
                        index: Box::new(ident(&idx_name)),
                        location: loc.clone(),
                    }),
                    is_private: None,
                    is_shared: None,
                    location: loc.clone(),
                }));
                Expression::BinaryExpression(ast::BinaryExpression {
                    left: Box::new(ident(&idx_name)),
                    operator: "<".to_string(),
                    right: Box::new(Expression::CallExpression(ast::CallExpression {
                        callee: Box::new(ident("length")),
                        args: vec![ast::CallArgument {
                            label: None,
                            value: ident(&coll_name),
                            location: loc.clone(),
                        }],
                        location: loc.clone(),
                    })),
                    location: loc.clone(),
                })
            };
            for s in &fs.body {
                anf_async_stmt(s, r, counter, &mut wbody);
            }
            wbody.push(Statement::AssignmentStatement(ast::AssignmentStatement {
                target: ast::AssignmentTarget::Variables {
                    names: vec![idx_name.clone()],
                },
                value: Expression::BinaryExpression(ast::BinaryExpression {
                    left: Box::new(ident(&idx_name)),
                    operator: "+".to_string(),
                    right: Box::new(int_lit(1.0)),
                    location: loc.clone(),
                }),
                location: loc.clone(),
            }));
            out.push(Statement::WhileStatement(ast::WhileStatement {
                condition,
                body: wbody,
                location: loc.clone(),
            }));
        }
        Statement::ForStatement(fs) => {
            let items = atomize(&fs.items, counter, out);
            let body = anf_async_body(&fs.body, r, counter);
            out.push(Statement::ForStatement(ast::ForStatement {
                item_name: fs.item_name.clone(),
                items,
                body,
                location: fs.location.clone(),
            }));
        }
        Statement::TryStatement(ts) => {
            let try_body = anf_async_body(&ts.try_body, r, counter);
            let catch_body = anf_async_body(&ts.catch_body, r, counter);
            let finally_body = ts
                .finally_body
                .as_ref()
                .map(|b| anf_async_body(b, r, counter));
            out.push(Statement::TryStatement(ast::TryStatement {
                try_body,
                catch_name: ts.catch_name.clone(),
                catch_body,
                finally_body,
                location: ts.location.clone(),
            }));
        }
        other => out.push(other.clone()),
    }
}

/// Desugar an `if … else if … else …` chain into nested single-branch ifs
/// (the only shape the resume CFG lowers) while A-normalizing every condition
/// and body. Recurses on the tail so branch *k*'s condition is hoisted into the
/// `else` of branch *k−1* — i.e. only evaluated when the earlier conditions
/// were false, exactly as the original chain.
fn anf_if_chain(
    branches: &[fai_compiler::ast::IfBranch],
    else_body: Option<&[Statement]>,
    loc: &fai_compiler::ast::SourceLocation,
    r: &AsyncResolve<'_>,
    counter: &mut usize,
    out: &mut Vec<Statement>,
) {
    use fai_compiler::ast;
    let Some(head) = branches.first() else {
        if let Some(eb) = else_body {
            for s in eb {
                anf_async_stmt(s, r, counter, out);
            }
        }
        return;
    };
    let condition = if expr_has_user_call(&head.condition, r) {
        anf_atom(&head.condition, r, counter, out)
    } else {
        head.condition.clone()
    };
    let body = anf_async_body(&head.body, r, counter);
    let else_branch = if branches.len() > 1 {
        let mut nested = Vec::new();
        anf_if_chain(&branches[1..], else_body, loc, r, counter, &mut nested);
        Some(nested)
    } else {
        else_body.map(|eb| anf_async_body(eb, r, counter))
    };
    out.push(Statement::IfStatement(ast::IfStatement {
        branches: vec![ast::IfBranch {
            condition,
            body,
            location: head.location.clone(),
        }],
        else_branch,
        location: loc.clone(),
    }));
}

/// Reduce `expr` to an atom for the CFG: hoist nested async calls, and if the
/// (rewritten) expression is *itself* an async call, hoist that too — returning
/// the temp identifier that now holds its awaited value.
fn anf_atom(
    expr: &Expression,
    r: &AsyncResolve<'_>,
    counter: &mut usize,
    out: &mut Vec<Statement>,
) -> Expression {
    let e = anf_nested(expr, r, counter, out);
    if let Expression::CallExpression(c) = &e {
        if remote_call_args(&e).is_some()
            || host_op_call_args(&e, r).is_some()
            || offloadable_extern_call_args(&e).is_some()
            || user_callee(&e, r).is_some()
        {
            let loc = c.location.clone();
            let name = format!("__anf_await_{}", *counter);
            *counter += 1;
            out.push(Statement::LetStatement(fai_compiler::ast::LetStatement {
                bindings: vec![fai_compiler::ast::BindingDeclaration {
                    name: name.clone(),
                    type_name: None,
                }],
                value: e,
                is_private: None,
                is_shared: None,
                location: loc.clone(),
            }));
            return Expression::IdentifierExpression(fai_compiler::ast::IdentifierExpression {
                name,
                location: loc,
            });
        }
    }
    e
}

/// Hoist async calls in the proper-subexpression positions of `expr` (call
/// args, arithmetic operands, member/index objects, …). The top-level
/// expression is returned in place — even if it is itself an async call — so an
/// await-position caller keeps it; `anf_atom` hoists the top level when an atom
/// is required. Async-free expressions are returned unchanged.
fn anf_nested(
    expr: &Expression,
    r: &AsyncResolve<'_>,
    counter: &mut usize,
    out: &mut Vec<Statement>,
) -> Expression {
    // Closure literal: A-normalize its *body* (a separate function — hoisted
    // temps stay inside it, keyed off the shared counter for unique names). The
    // closure value itself is not a call to hoist. Done unconditionally because
    // a closure passed to an async-free host call (`server.get(r,'*') do … end`)
    // still needs its async body rewritten, and `expr_has_user_call` does not
    // descend into closures.
    if let Expression::FunctionExpression(fd) = expr {
        let mut fd2 = fd.clone();
        fd2.body = anf_async_body(&fd.body, r, counter);
        return Expression::FunctionExpression(fd2);
    }
    // Leaf atoms have nothing to rewrite.
    if matches!(
        expr,
        Expression::IdentifierExpression(_)
            | Expression::NumberExpression(_)
            | Expression::StringExpression(_)
            | Expression::BooleanExpression(_)
            | Expression::NullExpression(_)
    ) {
        return expr.clone();
    }
    // `all(...)` is a concurrency special form: its arguments are concurrent
    // spawn points lowered by the CFG's `Term::All`, not ordinary call args.
    // Hoisting them would serialize the spawns (and break `all_call` detection),
    // so leave it opaque — the CFG handles `let [a, b] = all(f(), g())` directly.
    if let Expression::CallExpression(c) = expr {
        if let Expression::IdentifierExpression(id) = &*c.callee {
            if id.name == "all" {
                return expr.clone();
            }
        }
    }
    let mut e = expr.clone();
    match &mut e {
        Expression::CallExpression(c) => {
            *c.callee = anf_atom(&c.callee, r, counter, out);
            for a in &mut c.args {
                a.value = anf_atom(&a.value, r, counter, out);
            }
        }
        Expression::BinaryExpression(b) if b.operator != "&&" && b.operator != "||" => {
            *b.left = anf_atom(&b.left, r, counter, out);
            *b.right = anf_atom(&b.right, r, counter, out);
        }
        Expression::UnaryExpression(u) => {
            *u.expression = anf_atom(&u.expression, r, counter, out);
        }
        Expression::MemberExpression(m) => {
            *m.object = anf_atom(&m.object, r, counter, out);
        }
        Expression::IndexExpression(i) => {
            *i.object = anf_atom(&i.object, r, counter, out);
            *i.index = anf_atom(&i.index, r, counter, out);
        }
        Expression::ArrayExpression(a) => {
            for it in &mut a.items {
                *it = anf_atom(it, r, counter, out);
            }
        }
        Expression::TupleExpression(t) => {
            for it in &mut t.items {
                *it = anf_atom(it, r, counter, out);
            }
        }
        Expression::OptionalCheckExpression(o) => {
            *o.expression = anf_atom(&o.expression, r, counter, out);
        }
        Expression::ForceUnwrapExpression(f) => {
            *f.expression = anf_atom(&f.expression, r, counter, out);
        }
        _ => {}
    }
    e
}

/// Try to compile an async program through the real engine. Returns
/// `Some(wasm)` only for the v1-handled shape (native target, no modules,
/// a single async `main` whose suspension is `sleep`); otherwise `None`
/// so the caller falls back to the existing path.
pub fn try_codegen_async_engine(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    checker: &CheckerInfo,
    target: Option<&str>,
    analysis: &crate::async_analysis::AsyncAnalysis,
    entry_file: Option<&str>,
    test_plans: Option<&[crate::test_surface::TestWrapperPlan]>,
) -> Option<Vec<u8>> {
    use crate::async_engine::{self, SchedLayout};
    use crate::runtime::{self, IMPORT_NOW_MS, RT_ALLOC, RT_COUNT, RT_FREE};
    use std::collections::HashMap as Map;
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ElementSection, Elements, EntityType, ExportKind,
        ExportSection, FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection,
        MemoryType, Module as EncModule, RefType, TableSection, TableType, TypeSection,
    };

    clear_last_async_engine_error();

    // ── v1 gate ──
    // (A4) Browser targets now engage the real engine too. `sleep` arranges a
    // host wakeup via `host_set_timer` instead of the native busy-poll.
    // Engage when the program has any async at all (something suspends, or
    // there's a `nowait` fork). `main` itself need not suspend — it may just
    // fork a `nowait` task. Purely-sync programs have an empty analysis and
    // fall through to the sync path.
    if analysis.is_empty() {
        return None;
    }
    // Test build (plan 103 U6): the caller injected one wrapper function per
    // (suite, case) via `test_surface::inject_test_wrappers`; each becomes a
    // spawnable root task reachable through `_fai_spawn_test`.
    let is_test = test_plans.is_some();
    let wrapper_roots: Vec<(String, u16, u16)> = test_plans
        .unwrap_or(&[])
        .iter()
        .map(|p| {
            let name = match &p.module {
                Some(m) => format!("{}.{}", m, p.fn_name),
                None => p.fn_name.clone(),
            };
            (name, p.suite_idx, p.case_idx)
        })
        .collect();
    // Gather every user function from the entry AST and every module. Module
    // functions are name-prefixed `{module}.{fn}` exactly as `build_program_full`
    // and `async_analysis` do, so the analysis' qualified async set and the
    // function table agree. `decls` owns the (possibly renamed) declarations;
    // `fn_module` records each one's module context for call resolution.
    // Each decl carries (fn, module_context, file_path). The file path feeds
    // the per-call-site `module_key` (UFCS / named-param / expression-type
    // lookups) — it must match what the checker recorded, exactly as
    // `build_program_full` plumbs it.
    let mut decls: Vec<(FunctionDeclaration, Option<String>, Option<String>)> = Vec::new();
    for s in &ast.statements {
        if let Statement::FunctionDeclaration(fd) = s {
            decls.push((fd.clone(), None, None));
        }
    }
    for m in modules {
        for (idx, s) in m.statements.iter().enumerate() {
            if let Statement::FunctionDeclaration(fd) = s {
                let mut prefixed = fd.clone();
                prefixed.name = format!("{}.{}", m.name, fd.name);
                let file = m.file_paths.get(idx).cloned().flatten();
                decls.push((prefixed, Some(m.name.clone()), file));
            }
        }
    }
    if decls.is_empty() {
        return None;
    }

    // ── module-level `var NAME = EXPR` globals + their initializers ──
    // Their globals live after the 4 runtime + 7 scheduler globals, so they
    // start at index 11. Initializers run once, before `main` is spawned, via
    // a synthesized `<__module_init__>` that the scheduler's `start_async`
    // calls — one per module context so each resolves its own imports.
    // Globals 0..=3 are runtime (heap_ptr, env_ptr, error_flag, error_value);
    // 4..=11 are the scheduler (g_count..g_free_head). Module `var`s follow.
    const MODULE_VAR_BASE: u32 = 12;
    let mut module_vars: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut module_var_inits: Vec<(Option<String>, Statement)> = Vec::new();
    {
        let mut collect = |stmts: &[Statement], ctx_mod: Option<&str>| {
            for s in stmts {
                if let Statement::VarStatement(vs) = s {
                    if vs.bindings.len() != 1 {
                        continue;
                    }
                    let name = &vs.bindings[0].name;
                    if module_vars.contains_key(name) {
                        continue;
                    }
                    module_vars.insert(
                        name.clone(),
                        MODULE_VAR_BASE + module_var_inits.len() as u32,
                    );
                    let target = fai_compiler::ast::AssignmentTarget::Variables {
                        names: vec![name.clone()],
                    };
                    let assign =
                        Statement::AssignmentStatement(fai_compiler::ast::AssignmentStatement {
                            target,
                            value: vs.value.clone(),
                            location: vs.location.clone(),
                        });
                    module_var_inits.push((ctx_mod.map(|s| s.to_string()), assign));
                }
            }
        };
        collect(&ast.statements, None);
        for m in modules {
            collect(&m.statements, Some(m.name.as_str()));
        }
    }
    let module_var_count = module_var_inits.len() as u32;
    let mut master_init_name: Option<String> = None;
    if module_var_count > 0 {
        let loc = fai_compiler::ast::SourceLocation { line: 0, column: 0 };
        let synth = |name: String, body: Vec<Statement>| FunctionDeclaration {
            name,
            type_params: Vec::new(),
            params: Vec::new(),
            return_types: Vec::new(),
            body,
            doc: None,
            is_private: None,
            is_abstract: false,
            is_remote: false,
            auth_policy: None,
            location: loc.clone(),
            doc_comment: None,
        };
        let mk_call = |name: &str| {
            Statement::ExpressionStatement(fai_compiler::ast::ExpressionStatement {
                expression: Expression::CallExpression(fai_compiler::ast::CallExpression {
                    callee: Box::new(Expression::IdentifierExpression(
                        fai_compiler::ast::IdentifierExpression {
                            name: name.to_string(),
                            location: loc.clone(),
                        },
                    )),
                    args: Vec::new(),
                    location: loc.clone(),
                }),
                location: loc.clone(),
            })
        };
        // Group initializers by module context, in first-seen order.
        let mut groups: Vec<(Option<String>, Vec<Statement>)> = Vec::new();
        for (ctx_mod, stmt) in &module_var_inits {
            match groups.iter_mut().find(|(m, _)| m == ctx_mod) {
                Some((_, v)) => v.push(stmt.clone()),
                None => groups.push((ctx_mod.clone(), vec![stmt.clone()])),
            }
        }
        let mut master_body: Vec<Statement> = Vec::new();
        for (ctx_mod, body) in groups {
            let fn_name = match &ctx_mod {
                Some(m) => format!("<__module_init__:{}>", m),
                None => "<__module_init__:>".to_string(),
            };
            master_body.push(mk_call(&fn_name));
            decls.push((synth(fn_name, body), ctx_mod, None));
        }
        decls.push((
            synth("<__module_init__>".to_string(), master_body),
            None,
            None,
        ));
        master_init_name = Some("<__module_init__>".to_string());
    }

    // name -> (module_context, file_path) for per-fn call resolution + module_key.
    let fn_ctx: std::collections::HashMap<String, (Option<String>, Option<String>)> = decls
        .iter()
        .map(|(fd, m, f)| (fd.name.clone(), (m.clone(), f.clone())))
        .collect();

    // ── module context maps (mirror build_program_full / async_analysis) ──
    let mut module_fn_exports: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for m in modules {
        let mut names = Vec::new();
        for s in &m.statements {
            if let Statement::FunctionDeclaration(fd) = s {
                if !m.private_names.iter().any(|n| n == &fd.name) {
                    names.push(fd.name.clone());
                }
            }
        }
        module_fn_exports.insert(m.name.clone(), names);
    }
    let mut module_aliases: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    {
        let mut basename_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for m in modules {
            if let Some(last) = m.name.rsplit('.').next() {
                *basename_counts.entry(last.to_string()).or_insert(0) += 1;
            }
        }
        for m in modules {
            if let Some(last) = m.name.rsplit('.').next() {
                if basename_counts.get(last).copied().unwrap_or(0) == 1 {
                    module_aliases.insert(last.to_string(), m.name.clone());
                }
            }
        }
    }
    for (k, v) in collect_module_aliases_from(None, &ast.statements) {
        module_aliases.insert(k, v);
    }
    for m in modules {
        for (k, v) in collect_module_aliases_from(Some(&m.name), &m.statements) {
            module_aliases.entry(k).or_insert(v);
        }
    }
    let mut named_imports: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    {
        let mut record = |stmts: &[Statement], current: Option<&str>, entry_wins: bool| {
            for s in stmts {
                let Statement::UseStatement(u) = s else {
                    continue;
                };
                let qualified = qualify_module_path_for_codegen(current, &u.module_path);
                let put =
                    |out: &mut std::collections::HashMap<String, String>, k: String, v: String| {
                        if entry_wins {
                            out.insert(k, v);
                        } else {
                            out.entry(k).or_insert(v);
                        }
                    };
                if u.import_all {
                    if fai_checker::std_modules::is_std_module(&u.module_path) {
                        if let Some(exports) =
                            fai_checker::std_modules::std_module_exports().get(&qualified)
                        {
                            for (n, _) in exports {
                                put(
                                    &mut named_imports,
                                    n.clone(),
                                    format!("{}.{}", qualified, n),
                                );
                            }
                        }
                    } else if let Some(names) = module_fn_exports.get(&qualified) {
                        for n in names {
                            put(
                                &mut named_imports,
                                n.clone(),
                                format!("{}.{}", qualified, n),
                            );
                        }
                    }
                } else if let Some(names) = &u.imported_names {
                    for n in names {
                        put(
                            &mut named_imports,
                            n.clone(),
                            format!("{}.{}", qualified, n),
                        );
                    }
                }
            }
        };
        record(&ast.statements, None, true);
        for m in modules {
            record(&m.statements, Some(&m.name), false);
        }
    }
    // ── hybrid model ──
    // Only async-effectful functions become resume tasks; everything else
    // stays on the fast sync path (compiled by `build_function`). The async
    // set is the analysis' async ∪ scheduler functions. A call to an async fn
    // is an await; a call to a sync fn is a plain direct call.
    let mut async_set: std::collections::HashSet<String> = analysis
        .async_functions
        .iter()
        .chain(analysis.scheduler_functions.iter())
        .cloned()
        .collect();
    // Names of every user function (for module-aware call resolution). For a
    // single file these are bare names; module fns are `{module}.{fn}`.
    let all_user_fns: std::collections::HashSet<String> =
        decls.iter().map(|(fd, _, _)| fd.name.clone()).collect();
    let mut reachable_functions = analysis.reachable_functions.clone();
    if all_user_fns.contains("main") {
        reachable_functions.insert("main".to_string());
    }
    for (name, _, _) in &wrapper_roots {
        reachable_functions.insert(name.clone());
    }
    if let Some(name) = &master_init_name {
        reachable_functions.insert(name.clone());
        for (fd, _, _) in &decls {
            if fd.name.starts_with("<__module_init__:") {
                reachable_functions.insert(fd.name.clone());
            }
        }
    }
    // A spawned function (`nowait f()` / `all(f(), ...)`) must be a resume
    // task even if its own body never suspends — fold those targets in
    // (resolved to their canonical names in each fn's module context).
    {
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut targets: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (fd, mctx, fctx) in &decls {
            if !reachable_functions.contains(&fd.name) {
                continue;
            }
            let mk = fctx.as_deref().or(mctx.as_deref()).unwrap_or("");
            let r = AsyncResolve {
                async_set: &empty,
                all_fns: &all_user_fns,
                aliases: &module_aliases,
                named_imports: &named_imports,
                module_context: mctx.as_deref(),
                ufcs_calls: &checker.ufcs_calls,
                module_key: mk,
            };
            collect_spawn_targets(&fd.body, &r, &mut targets);
        }
        reachable_functions.extend(targets.iter().cloned());
        async_set.extend(targets);
    }
    async_set.retain(|name| reachable_functions.contains(name));
    // main must exist and take no arguments — except in a test build, where
    // the roots are the case wrappers and `main` (if present) is just another
    // function.
    let has_main = decls.iter().any(|(fd, _, _)| fd.name == "main");
    if !is_test {
        let main_decl = decls.iter().find(|(fd, _, _)| fd.name == "main")?;
        if !main_decl.0.params.is_empty() {
            return None; // root takes no arguments
        }
    }
    // A1 — "everything is async, even `main`." `main` is always the startup
    // root task, whether or not its own body suspends. (Pure-sync programs
    // never reach here: the `analysis.is_empty()` early-out above sends them to
    // the fast path.)
    if has_main {
        async_set.insert("main".to_string());
    }
    // Every case wrapper is spawned as a task by the runner, so it must be a
    // resume fn even when its body never suspends (mirrors spawn targets).
    for (name, _, _) in &wrapper_roots {
        async_set.insert(name.clone());
    }

    // ── A-normalize async calls ──
    // The CFG's await lowering only recognizes an async call when it is the
    // whole value of a `let`/`return`/expr-stmt. Hoist async calls nested as
    // subexpressions (`print(component())`, `f(asyncG())`, …) into preceding
    // `let __anf_await_N = <call>` temps so they lower as awaits. Done before
    // `all_fns`/frame layout so every downstream pass sees the rewritten bodies.
    // `async_set` was computed from the original bodies — still correct, since
    // hoisting reorders calls into temps but changes neither the call graph nor
    // which functions are async.
    {
        for (fd, mctx, fctx) in decls.iter_mut() {
            let mk = fctx.as_deref().or(mctx.as_deref()).unwrap_or("");
            let r = AsyncResolve {
                async_set: &async_set,
                all_fns: &all_user_fns,
                aliases: &module_aliases,
                named_imports: &named_imports,
                module_context: mctx.as_deref(),
                ufcs_calls: &checker.ufcs_calls,
                module_key: mk,
            };
            let mut counter = 0usize;
            let rewritten = anf_async_body(&fd.body, &r, &mut counter);
            fd.body = rewritten;
        }
    }

    let all_fns: Vec<&FunctionDeclaration> = decls.iter().map(|(fd, _, _)| fd).collect();
    let main: Option<&FunctionDeclaration> = match all_fns.iter().find(|fd| fd.name == "main") {
        Some(m) => Some(*m),
        None if is_test => None,
        None => return None,
    };
    for fd in &all_fns {
        if !reachable_functions.contains(&fd.name) {
            continue;
        }
        let is_async = async_set.contains(&fd.name);
        // A sync fn becomes a `FaiFunc(arity)` in the table-type space.
        if !is_async && (fd.params.len() + fd.type_params.len()) > MAX_DIRECT_ARITY as usize {
            return None;
        }
    }

    // Proto order = wasm function order: each user fn sits at
    // `import_count + RT_COUNT + proto`. `main` is first (when present).
    let mut ordered: Vec<&FunctionDeclaration> = Vec::new();
    if let Some(m) = main {
        ordered.push(m);
    }
    let mut rest: Vec<&FunctionDeclaration> = all_fns
        .iter()
        .copied()
        .filter(|fd| fd.name != "main" && reachable_functions.contains(&fd.name))
        .collect();
    rest.sort_by(|a, b| a.name.cmp(&b.name));
    ordered.extend(rest);

    // Table slots go to async fns only (sync fns are called directly, never
    // through the table). `main` = slot 0 (root). Frames likewise exist only
    // for async fns.
    let mut fn_table_idx: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut frame_sizes: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    let mut frames: std::collections::HashMap<String, AsyncFrame> =
        std::collections::HashMap::new();
    let mut tpos = 0u32;
    for fd in &ordered {
        if async_set.contains(&fd.name) {
            let (mctx, fctx) = fn_ctx
                .get(&fd.name)
                .map(|(m, f)| (m.as_deref(), f.as_deref()))
                .unwrap_or((None, None));
            let r = AsyncResolve {
                async_set: &async_set,
                all_fns: &all_user_fns,
                aliases: &module_aliases,
                named_imports: &named_imports,
                module_context: mctx,
                ufcs_calls: &checker.ufcs_calls,
                module_key: fctx.or(mctx).unwrap_or(""),
            };
            let frame = async_frame_layout(fd, &r, false);
            fn_table_idx.insert(fd.name.clone(), tpos);
            frame_sizes.insert(fd.name.clone(), frame.size);
            frames.insert(fd.name.clone(), frame);
            tpos += 1;
        }
    }
    let nasync = tpos;
    let nuser = ordered.len() as u32;
    // Test wrappers must all have resume-table slots by now; map each
    // (suite, case) to its wrapper's table index + frame size for
    // `_fai_spawn_test` (plan 103 U6).
    let mut spawn_cases: Vec<async_engine::SpawnTestCase> = Vec::new();
    for (name, suite, case) in &wrapper_roots {
        let table_idx = *fn_table_idx.get(name)?;
        let frame_size = *frame_sizes.get(name)?;
        spawn_cases.push(async_engine::SpawnTestCase {
            suite: *suite,
            case: *case,
            table_idx,
            frame_size,
        });
    }
    let root_frame_size = match frames.get("main") {
        Some(f) => f.size,
        None if is_test => 0, // no root spawn in test builds (spawn_root=false)
        None => return None,
    };

    // ── module-level index layout ──
    let import_available = runtime::available_imports_with_test_flag(target, is_test);
    let (import_remap, actual_import_count) = runtime::build_import_remap(&import_available);
    let now_ms_idx = import_remap
        .get(IMPORT_NOW_MS as usize)
        .copied()
        .flatten()?;
    let import_sigs = runtime::import_signatures();
    let rt_sigs = runtime::type_signatures();

    // Scheduler-specific function types are appended after import, rt, and
    // FaiFunc(0..=MAX_DIRECT_ARITY) types.
    let sched_type_base =
        (import_sigs.len() + rt_sigs.len() + (MAX_DIRECT_ARITY as usize + 1)) as u32;
    let t_resume = sched_type_base;
    let t_i32_void = sched_type_base + 1;
    let t_void_i32 = sched_type_base + 2;
    let t_i32i32_i32 = sched_type_base + 3;
    let t_i32i64_void = sched_type_base + 4;
    let t_i32f64_void = sched_type_base + 5;
    let t_i32_i32 = sched_type_base + 6;
    let t_i32_i64 = sched_type_base + 7;
    let t_i32i32_void = sched_type_base + 8;
    let t_i64i64_i64 = sched_type_base + 9; // __fai_drive_closure

    // User fns occupy `[import_count + RT_COUNT, +nuser)` so async→sync direct
    // calls resolve to `rt.base + RT_COUNT + proto`. The scheduler sits after.
    let user_fn_base = actual_import_count + RT_COUNT;
    let sb = user_fn_base + nuser; // first scheduler fn index
                                   // Wasm index of the synthesized module-init fn (if any), for start_async.
    let module_init = master_init_name.as_ref().and_then(|name| {
        ordered
            .iter()
            .position(|fd| &fd.name == name)
            .map(|proto| user_fn_base + proto as u32)
    });
    let layout = SchedLayout {
        now_ms: now_ms_idx,
        alloc: actual_import_count + RT_ALLOC,
        free: actual_import_count + RT_FREE,
        retain: actual_import_count + RT_RETAIN,
        release: actual_import_count + RT_RELEASE,
        ready_push: sb,
        ready_pop: sb + 1,
        spawn: sb + 2,
        complete: sb + 3,
        fail: sb + 4,
        sleep: sb + 5,
        notify: sb + 6,
        poll: sb + 7,
        resume_task: sb + 9,
        task_result: sb + 10,
        await_fn: sb + 11,
        resume_type: t_resume,
        g_count: 4,
        g_head: 5,
        g_tail: 6,
        g_root: 7,
        g_current: 8,
        g_table_base: 9,
        g_live: 10,
        g_free_head: 11,
        // Appended after heap debug globals so existing module-var global
        // indices stay stable.
        g_timer_waiting: 14 + module_var_count,
        g_completed_head: 15 + module_var_count,
        g_completed_tail: 16 + module_var_count,
        main_resume_table_idx: 0,
        capacity: 4096,
        root_frame_size,
        module_init,
        // Browser targets delegate sleep wakeups to the host timer; native
        // busy-polls. `host_set_timer` is available on all targets, so gate on
        // the target rather than mere availability.
        set_timer: if matches!(target, Some("wasm") | Some("wasm-html")) {
            import_remap
                .get(crate::runtime::IMPORT_HOST_SET_TIMER as usize)
                .copied()
                .flatten()
        } else {
            None
        },
        // Native keeps guest-owned promotion (O_WAKE + now_ms in poll) but
        // reports each deadline to the host so the driver's park wakes when
        // the nearest timer is due, not at the backstop (plan 103 U4).
        set_timer_hint: if matches!(target, Some("wasm") | Some("wasm-html")) {
            None
        } else {
            import_remap
                .get(crate::runtime::IMPORT_HOST_SET_TIMER as usize)
                .copied()
                .flatten()
        },
        trap_report: import_remap
            .get(crate::runtime::IMPORT_TRAP_REPORT as usize)
            .copied()
            .flatten(),
        // Test builds spawn cases individually via `_fai_spawn_test`.
        spawn_root: !is_test,
    };
    let start_async_idx = sb + 8;

    // ── compile each async function as a resume function ──
    let rt = RtOffsets {
        base: actual_import_count,
    };
    let fai_type_indices = direct_fai_func_type_indices();
    let strings = RefCell::new(StringInterner::default());
    let closures = RefCell::new(Vec::new());

    // ── context maps from entry + every module (mirror build_program_full) ──
    let mut enum_members: HashMap<String, Vec<String>> = HashMap::new();
    let mut type_fields: HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>> = HashMap::new();
    let mut module_constants: HashMap<String, fai_compiler::ast::Expression> = HashMap::new();
    let mut extern_fn_indices = collect_extern_fn_indices_from(&ast.statements);
    OFFLOADABLE_EXTERNS.with(|m| *m.borrow_mut() = offloadable_extern_indices(&ast.statements));
    let mut extern_out_params: HashMap<String, Vec<bool>> = HashMap::new();
    fn collect_consts(
        stmts: &[Statement],
        out: &mut HashMap<String, fai_compiler::ast::Expression>,
    ) {
        for s in stmts {
            if let Statement::LetStatement(ls) = s {
                if ls.bindings.len() == 1
                    && matches!(
                        ls.value,
                        Expression::NumberExpression(_)
                            | Expression::BooleanExpression(_)
                            | Expression::NullExpression(_)
                            | Expression::StringExpression(_)
                    )
                {
                    out.entry(ls.bindings[0].name.clone())
                        .or_insert_with(|| ls.value.clone());
                }
            }
        }
    }
    let mut collect_decls = |stmts: &[Statement]| {
        for s in stmts {
            match s {
                Statement::EnumDeclaration(ed) => {
                    enum_members
                        .entry(ed.name.clone())
                        .or_insert_with(|| ed.members.clone());
                }
                Statement::TypeDeclaration(td) => {
                    type_fields
                        .entry(td.name.clone())
                        .or_insert_with(|| td.fields.clone());
                }
                Statement::ExternBlockDeclaration(ext) => {
                    for f in &ext.functions {
                        extern_out_params
                            .entry(f.name.clone())
                            .or_insert_with(|| f.params.iter().map(|p| p.is_out).collect());
                    }
                }
                _ => {}
            }
        }
        collect_consts(stmts, &mut module_constants);
    };
    collect_decls(&ast.statements);
    for m in modules {
        collect_decls(&m.statements);
    }
    // Externs from modules get fresh indices after the entry's.
    let mut next_ext = extern_fn_indices
        .values()
        .max()
        .map(|m| *m + 1)
        .unwrap_or(0);
    for m in modules {
        for s in &m.statements {
            if let Statement::ExternBlockDeclaration(ext) = s {
                for f in &ext.functions {
                    extern_fn_indices.entry(f.name.clone()).or_insert_with(|| {
                        let i = next_ext;
                        next_ext += 1;
                        i
                    });
                }
            }
        }
    }
    for (name, fields) in builtin_type_fields() {
        type_fields.entry(name).or_insert(fields);
    }
    let infos: Vec<FunctionInfo> = ordered
        .iter()
        .map(|fd| FunctionInfo {
            name: fd.name.clone(),
            param_count: fd.params.len() as u16 + fd.type_params.len() as u16,
            type_param_count: fd.type_params.len() as u16,
            param_names: param_names_for(fd),
            include_in_coverage: false,
            param_defaults: param_defaults_for(fd),
            // Same fallback policy as `build_program_full`: entry-AST
            // functions (no module context) get the entry file.
            source_file: match fn_ctx.get(&fd.name) {
                Some((_, Some(f))) => Some(f.clone()),
                Some((None, None)) if fd.location.line > 0 => entry_file.map(String::from),
                _ => None,
            },
            source_line: fd.location.line,
        })
        .collect();
    // Spy/mock instrumentation (plan 103 U6): in test builds, functions that
    // a `test` block mocks or spies get the `spy_check_call` preamble via
    // `build_function_with_spy_and_offset` (sync fns). Async (resume-fn)
    // targets are NOT instrumented yet — mocking an async function under the
    // engine is a known v1 gap surfaced by the fixture audit.
    let function_by_name: Map<String, u32> = infos
        .iter()
        .enumerate()
        .map(|(i, info)| (info.name.clone(), i as u32))
        .collect();
    let spy_targets: SpyTargets = if is_test {
        collect_spy_targets(
            ast,
            modules,
            &function_by_name,
            &module_aliases,
            &named_imports,
        )
    } else {
        SpyTargets::default()
    };
    let mocked_fn_ids = spy_targets.fn_ids;
    let std_method_fn_ids = spy_targets.std_method_fn_ids;
    let ownership_sites = RefCell::new(Vec::new());
    let ctx = BuildContext {
        rt,
        functions: &infos,
        checker,
        import_remap: &import_remap,
        fai_func_type_indices: &fai_type_indices,
        module_aliases: &module_aliases,
        extern_fn_indices: &extern_fn_indices,
        enum_members: &enum_members,
        type_fields: &type_fields,
        named_imports: &named_imports,
        mocked_fn_ids: &mocked_fn_ids,
        std_method_fn_ids: &std_method_fn_ids,
        // Closures created inside async resume fns get table slots after the
        // async resume fns (which occupy 0..nasync).
        closure_offset_base: nasync,
        strings: &strings,
        closures: &closures,
        module_constants: &module_constants,
        extern_out_params: &extern_out_params,
        module_vars: &module_vars,
        ownership_sites: &ownership_sites,
        file_path: None,
        async_ctx: Some(AsyncClosureCtx {
            async_set: &async_set,
            all_fns: &all_user_fns,
            layout: &layout,
            fn_table_idx: &fn_table_idx,
            frame_sizes: &frame_sizes,
        }),
    };
    // Compile in two passes so closures get non-overlapping table slots:
    // async resume fns first (their closures fill slots `nasync..`, via the
    // shared `closures` RefCell at `closure_offset_base = nasync`), then sync
    // fns (their closures continue after the async ones). Bodies are placed by
    // proto index so the function/code sections stay in proto order.
    let mut bodies: Vec<Option<Function>> = (0..ordered.len()).map(|_| None).collect();
    for (proto, fd) in ordered.iter().enumerate() {
        if async_set.contains(&fd.name) {
            let (mctx, fctx) = fn_ctx
                .get(&fd.name)
                .map(|(m, f)| (m.as_deref(), f.as_deref()))
                .unwrap_or((None, None));
            let frame = &frames[&fd.name];
            let r = AsyncResolve {
                async_set: &async_set,
                all_fns: &all_user_fns,
                aliases: &module_aliases,
                named_imports: &named_imports,
                module_context: mctx,
                ufcs_calls: &checker.ufcs_calls,
                module_key: fctx.or(mctx).unwrap_or(""),
            };
            let (f, _upvalues) = match build_resume_fn(
                &ctx,
                fd,
                frame,
                &fn_table_idx,
                &frame_sizes,
                &layout,
                &r,
                mctx,
                fctx,
                None,
            ) {
                Ok(v) => v,
                Err(e) => {
                    if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
                        eprintln!("[async-engine] resume fn '{}' failed: {:?}", fd.name, e);
                    }
                    record_async_engine_error(e, fd, mctx, fctx, entry_file);
                    return None;
                }
            };
            bodies[proto] = Some(f);
        }
    }
    let async_closure_count = closures.borrow().len() as u32;
    let mut sync_closures: Vec<BuiltClosure> = Vec::new();
    for (proto, fd) in ordered.iter().enumerate() {
        if async_set.contains(&fd.name) {
            continue;
        }
        let (mctx, fctx) = fn_ctx
            .get(&fd.name)
            .map(|(m, f)| (m.as_deref(), f.as_deref()))
            .unwrap_or((None, None));
        let res = build_function_with_spy_and_offset(
            fd,
            rt,
            &infos,
            checker,
            &fai_type_indices,
            &module_aliases,
            &extern_fn_indices,
            &import_remap,
            &strings,
            &enum_members,
            &type_fields,
            &named_imports,
            &mocked_fn_ids,
            &std_method_fn_ids,
            nasync + async_closure_count + sync_closures.len() as u32,
            mctx,
            &module_constants,
            &extern_out_params,
            &module_vars,
            &ownership_sites,
            fctx,
            ctx.async_ctx,
        );
        let res = match res {
            Ok(v) => v,
            Err(e) => {
                if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
                    eprintln!("[async-engine] sync fn '{}' failed: {:?}", fd.name, e);
                }
                record_async_engine_error(e, fd, mctx, fctx, entry_file);
                return None;
            }
        };
        bodies[proto] = Some(res.main);
        sync_closures.extend(res.closures);
    }
    let bodies: Vec<Function> = bodies.into_iter().map(|b| b.unwrap()).collect();
    // All closures, in table-slot order: async-fn closures first (slots
    // `nasync..`), then sync-fn closures.
    let async_closures = closures.into_inner();
    let closure_count = async_closure_count + sync_closures.len() as u32;

    // ── data section (string pool + known strings) ──
    let mut extended = strings.into_inner().bytes;
    fn append_known(buf: &mut Vec<u8>, s: &str) -> (u32, u32) {
        let off = buf.len() as u32;
        buf.extend_from_slice(s.as_bytes());
        (off, s.len() as u32)
    }
    let str_null = append_known(&mut extended, "null");
    let str_true = append_known(&mut extended, "true");
    let str_false = append_known(&mut extended, "false");
    let known = runtime::KnownStrings {
        str_null,
        str_true,
        str_false,
        ..Default::default()
    };

    // ── assemble ──
    let mut module = EncModule::new();

    let mut types = TypeSection::new();
    for (_, p, r) in &import_sigs {
        types.ty().function(p.clone(), r.clone());
    }
    for (p, r) in &rt_sigs {
        types.ty().function(p.clone(), r.clone());
    }
    for arity in 0..=MAX_DIRECT_ARITY {
        let params: Vec<ValType> = (0..arity).map(|_| ValType::I64).collect();
        types.ty().function(params, vec![ValType::I64]);
    }
    types.ty().function(vec![], vec![]); // t_resume
    types.ty().function(vec![ValType::I32], vec![]); // t_i32_void
    types.ty().function(vec![], vec![ValType::I32]); // t_void_i32
    types
        .ty()
        .function(vec![ValType::I32, ValType::I32], vec![ValType::I32]); // t_i32i32_i32
    types
        .ty()
        .function(vec![ValType::I32, ValType::I64], vec![]); // t_i32i64_void
    types
        .ty()
        .function(vec![ValType::I32, ValType::F64], vec![]); // t_i32f64_void
    types.ty().function(vec![ValType::I32], vec![ValType::I32]); // t_i32_i32
    types.ty().function(vec![ValType::I32], vec![ValType::I64]); // t_i32_i64
    types
        .ty()
        .function(vec![ValType::I32, ValType::I32], vec![]); // t_i32i32_void
    types
        .ty()
        .function(vec![ValType::I64, ValType::I64], vec![ValType::I64]); // t_i64i64_i64
    module.section(&types);

    let mut imports = ImportSection::new();
    for (i, (name, _, _)) in import_sigs.iter().enumerate() {
        if import_available[i] {
            imports.import("env", name, EntityType::Function(i as u32));
        }
    }
    module.section(&imports);

    let mut funcs = FunctionSection::new();
    let rt_type_start = import_sigs.len() as u32;
    for k in 0..RT_COUNT {
        funcs.function(rt_type_start + k);
    }
    // User fns, proto order: async = resume type, sync = FaiFunc(arity).
    for fd in &ordered {
        if async_set.contains(&fd.name) {
            funcs.function(t_resume);
        } else {
            let pc = fd.params.len() as u16 + fd.type_params.len() as u16;
            funcs.function(fai_type_indices[&pc]);
        }
    }
    funcs.function(t_i32_void); // ready_push
    funcs.function(t_void_i32); // ready_pop
    funcs.function(t_i32i32_i32); // spawn
    funcs.function(t_i32i64_void); // complete
    funcs.function(t_i32i64_void); // fail
    funcs.function(t_i32f64_void); // sleep
    funcs.function(t_i32_void); // notify
    funcs.function(t_void_i32); // poll
    funcs.function(t_void_i32); // start_async
    funcs.function(t_i32_i32); // resume_task
    funcs.function(t_i32_i64); // task_result
    funcs.function(t_i32i32_void); // await
    funcs.function(t_i64i64_i64); // drive_closure
    funcs.function(t_void_i32); // completed_pop
                                // Closures, after the scheduler: async closures are resume fns (`t_resume`),
                                // sync closures are `FaiFunc(arity)`. async-fn closures first, then sync-fn,
                                // matching the table-slot order.
    for c in async_closures.iter().chain(sync_closures.iter()) {
        if c.is_async {
            funcs.function(t_resume);
        } else {
            funcs.function(fai_type_indices[&c.info.param_count]);
        }
    }
    // Appended after the closures so their indices never shift: the unified
    // host driver loop's spawn-without-drive and task-status entries (plan 101).
    funcs.function(t_i64i64_i64); // __fai_spawn_closure (i64,i64)->i64
    funcs.function(t_i64i64_i64); // __fai_spawn_queued_closure (i64,i64)->i64
    funcs.function(t_i32_i32); // __fai_task_status (i32)->i32
    funcs.function(t_i32_void); // __fai_free_task (i32)->()
    if is_test {
        funcs.function(t_i32i32_i32); // _fai_spawn_test (i32,i32)->i32
    }
    module.section(&funcs);

    // Table = [async resume fns (0..nasync)] ++ [sync-fn closures (nasync..)].
    let table_len = nasync + closure_count;
    let mut tables = TableSection::new();
    tables.table(TableType {
        element_type: RefType::FUNCREF,
        minimum: table_len as u64,
        maximum: Some(table_len as u64),
        table64: false,
        shared: false,
    });
    module.section(&tables);

    let total_bytes = extended.len() as u32 + runtime::FREE_BUCKET_REGION_BYTES + 65536;
    let pages = std::cmp::max((total_bytes / 65536) + 1, 16);
    let mut mem = MemorySection::new();
    mem.memory(MemoryType {
        minimum: pages as u64,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&mem);

    // The size-bucketed free-list heads live in a zero-init region starting at
    // `bucket_base`; the heap bump pointer starts just past it.
    let bucket_base = ((extended.len() as u32) + 7) & !7;
    let heap_start = (bucket_base + runtime::FREE_BUCKET_REGION_BYTES + 7) & !7;
    let i32mut = GlobalType {
        val_type: ValType::I32,
        mutable: true,
        shared: false,
    };
    let mut globals = GlobalSection::new();
    globals.global(i32mut, &ConstExpr::i32_const(heap_start as i32)); // __heap_ptr
    globals.global(i32mut, &ConstExpr::i32_const(0)); // __env_ptr
    globals.global(i32mut, &ConstExpr::i32_const(0)); // error_flag
    globals.global(
        GlobalType {
            val_type: ValType::I64,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i64_const(0),
    ); // error_value
       // Task ids start at 1: the host runner reads the root result via
       // `__fai_task_result(1)`, so `main` (the first spawn) must be id 1.
       // Slot 0 is left unused.
    globals.global(i32mut, &ConstExpr::i32_const(1)); // g_count
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_head
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_tail
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_root
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_current
    globals.global(i32mut, &ConstExpr::i32_const(0)); // g_table_base
    globals.global(i32mut, &ConstExpr::i32_const(0)); // g_live
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_free_head (empty)
                                                       // Module-level `var` globals (i64), indices 12.. — initialized to Void;
                                                       // their real values are written by `<__module_init__>` before `main` runs.
    let i64mut = GlobalType {
        val_type: ValType::I64,
        mutable: true,
        shared: false,
    };
    for _ in 0..module_var_count {
        globals.global(i64mut, &ConstExpr::i64_const(VAL_VOID));
    }
    // Heap free-list head for rt_alloc reuse / rt_free (appended last so the
    // fixed (0-3), scheduler (4-11) and module-var (12..) global indices are
    // unchanged). 0 = empty list. Index = 12 + module_var_count.
    globals.global(i32mut, &ConstExpr::i32_const(0));
    // Live-object counter (plan 113), appended after the free-list.
    globals.global(i32mut, &ConstExpr::i32_const(0));
    // Native timer wait count for skipping poll's task-table timer scan when
    // no timer waits exist. Appended after heap globals to avoid moving module
    // vars or existing debug globals.
    globals.global(i32mut, &ConstExpr::i32_const(0));
    // Host-queued completion FIFO for server handlers; appended to preserve all
    // earlier global indices.
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // completed_head
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // completed_tail
    // Ambient request-context id fallback (plan 133): the slot
    // taskContextId()/setTaskContextId() use when g_current is -1
    // (top level, before/after any task). Per-task O_CTX is used while a
    // task runs; this covers the no-current-task case so the pair is
    // coherent everywhere. -1 = none.
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // task_ctx_fallback
    module.section(&globals);

    // Heap free-list / live-count globals, appended after fixed+sched+
    // module-var globals (also referenced by the export section below).
    let freelist_global = 12 + module_var_count;
    let live_count_global = freelist_global + 1;
    let timer_waiting_global = live_count_global + 1;
    let completed_head_global = timer_waiting_global + 1;
    let completed_tail_global = completed_head_global + 1;
    let task_ctx_fallback_global = completed_tail_global + 1; // ambient ctx slot (plan 133)

    let mut exports = ExportSection::new();
    exports.export("_start_async", ExportKind::Func, start_async_idx);
    exports.export("__fai_poll", ExportKind::Func, sb + 7);
    exports.export("__fai_resume_task", ExportKind::Func, sb + 9);
    exports.export("__fai_task_result", ExportKind::Func, sb + 10);
    // Host-driver entry: spawn+drive an async guest closure (route/event handler)
    // to completion. await = sb+11, drive_closure = sb+12 (appended last).
    exports.export("__fai_drive_closure", ExportKind::Func, sb + 12);
    exports.export("__fai_pop_completed_task", ExportKind::Func, sb + 13);
    // Host-driver loop entries (plan 101), appended after the closures so the
    // closure table indices never shift. Scheduler block is sb..sb+13; closures
    // occupy sb+SCHED_FN_COUNT..+nclosures; these sit just past them.
    let nclosures = (async_closures.len() + sync_closures.len()) as u32;
    let host_driver_base = sb + async_engine::SCHED_FN_COUNT + nclosures;
    let spawn_closure_idx = host_driver_base;
    let spawn_queued_closure_idx = host_driver_base + 1;
    let task_status_idx = host_driver_base + 2;
    let free_task_idx = host_driver_base + 3;
    exports.export("__fai_spawn_closure", ExportKind::Func, spawn_closure_idx);
    exports.export(
        "__fai_spawn_queued_closure",
        ExportKind::Func,
        spawn_queued_closure_idx,
    );
    exports.export("__fai_task_status", ExportKind::Func, task_status_idx);
    exports.export("__fai_free_task", ExportKind::Func, free_task_idx);
    if is_test {
        exports.export("_fai_spawn_test", ExportKind::Func, free_task_idx + 1);
    }
    // Host-callable refcount helpers: lets the host retain guest handles it
    // stores and reclaim per-request guest objects it owns (the request/response
    // dicts it built) after writing the response, so a long-running server
    // plateaus instead of leaking retained route/event graphs (plans 115/117).
    exports.export(
        "__fai_retain",
        ExportKind::Func,
        actual_import_count + runtime::RT_RETAIN,
    );
    exports.export(
        "__fai_release",
        ExportKind::Func,
        actual_import_count + runtime::RT_RELEASE,
    );
    exports.export("memory", ExportKind::Memory, 0);
    // The host wasm runner allocates guest values (strings/arrays/dicts returned
    // by imports, FFI write-backs) by bumping `__heap_ptr`, and calls guest
    // closures back (event handlers, route handlers) through
    // `__indirect_function_table` after seeding `__env_ptr`. The sync path
    // exports these; the engine must too or the runner panics
    // (`heap.rs: get_export("__heap_ptr").unwrap()` on `None`). Global layout:
    // `__heap_ptr` = 0, `__env_ptr` = 1 (see `GLOBAL_ENV_PTR`); the function
    // table is table 0.
    exports.export("__heap_ptr", ExportKind::Global, 0);
    // Live-object counter (plan 113); index = free-list (12 + module vars) + 1.
    exports.export("__live_objects", ExportKind::Global, 13 + module_var_count);
    exports.export("__env_ptr", ExportKind::Global, GLOBAL_ENV_PTR);
    // The browser runtime signals a failed `remoteCall` by setting these from JS
    // (`instance.exports.__error_flag.value = 1`), so the awaiting guest task
    // observes the error after it resumes. The sync path exports them too.
    exports.export("__error_flag", ExportKind::Global, GLOBAL_ERROR_FLAG);
    exports.export("__error_value", ExportKind::Global, GLOBAL_ERROR_VALUE);
    exports.export("__indirect_function_table", ExportKind::Table, 0);
    // Scheduler-introspection globals (plan 116 phase 2): always exported
    // so the runner's post-mortem dump can walk the task table on a trap
    // or watchdog timeout without a special debug build.
    exports.export("__dbg_count", ExportKind::Global, layout.g_count);
    exports.export("__dbg_root", ExportKind::Global, layout.g_root);
    exports.export("__dbg_live", ExportKind::Global, layout.g_live);
    exports.export("__dbg_current", ExportKind::Global, layout.g_current);
    exports.export("__dbg_table_base", ExportKind::Global, layout.g_table_base);
    exports.export("__dbg_free_head", ExportKind::Global, layout.g_free_head);
    exports.export("__dbg_head", ExportKind::Global, layout.g_head);
    exports.export("__dbg_tail", ExportKind::Global, layout.g_tail);
    exports.export(
        "__dbg_timer_waiting",
        ExportKind::Global,
        timer_waiting_global,
    );
    exports.export(
        "__dbg_completed_head",
        ExportKind::Global,
        completed_head_global,
    );
    exports.export(
        "__dbg_completed_tail",
        ExportKind::Global,
        completed_tail_global,
    );
    // Heap overflow free-list head (blocks too large for the size
    // buckets) — the post-mortem heap stats walk it.
    exports.export("__free_list", ExportKind::Global, freelist_global);
    if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
        // TEMP brain: nextSignalId=g16, registeredSignals=g18, routerPathSignal=g28
        if module_var_count > 17 {
            exports.export("__dbg_g16", ExportKind::Global, 16);
            exports.export("__dbg_g18", ExportKind::Global, 18);
            exports.export("__dbg_g28", ExportKind::Global, 28);
        }
    }
    module.section(&exports);

    let mut elements = ElementSection::new();
    // Table: [0..nasync) async fns → `user_fn_base + proto`; then closures →
    // `sb + SCHED_FN_COUNT + i` (closures sit after the scheduler in code).
    let closure_base = sb + async_engine::SCHED_FN_COUNT;
    let mut table_fns = vec![0u32; table_len as usize];
    for (proto, fd) in ordered.iter().enumerate() {
        if let Some(&tp) = fn_table_idx.get(&fd.name) {
            table_fns[tp as usize] = user_fn_base + proto as u32;
        }
    }
    for i in 0..closure_count {
        table_fns[(nasync + i) as usize] = closure_base + i;
    }
    elements.active(
        Some(0),
        &ConstExpr::i32_const(0),
        Elements::Functions(table_fns.into()),
    );
    module.section(&elements);

    let mut code = CodeSection::new();
    for f in runtime::emit_all(
        actual_import_count,
        &import_remap,
        &known,
        freelist_global,
        live_count_global,
        bucket_base,
        Some(layout.g_current),
        Some((layout.g_table_base, layout.capacity)),
        task_ctx_fallback_global,
    ) {
        code.function(&f);
    }
    // User fns (proto order) — must match the function section ordering.
    for body in &bodies {
        code.function(body);
    }
    for f in async_engine::emit_scheduler_functions(&layout) {
        code.function(&f);
    }
    for c in async_closures.iter().chain(sync_closures.iter()) {
        code.function(&c.function);
    }
    // Appended after the closures (function-section order matches): host driver
    // loop entries (plan 101). Indices are computed in the export section.
    code.function(&async_engine::emit_spawn_closure(&layout));
    code.function(&async_engine::emit_spawn_queued_closure(&layout));
    code.function(&async_engine::emit_task_status(&layout));
    code.function(&async_engine::emit_free_task(&layout));
    if is_test {
        code.function(&async_engine::emit_spawn_test(&layout, &spawn_cases));
    }
    module.section(&code);

    if !extended.is_empty() {
        let mut data = DataSection::new();
        data.active(0, &ConstExpr::i32_const(0), extended.iter().copied());
        module.section(&data);
    }

    // ── debug metadata (plan 116): name section + fai-dbg table ──
    let mut dbg: Vec<crate::debug_info::FnDebugEntry> = Vec::new();
    for (i, (name, _, _)) in import_sigs.iter().enumerate() {
        if let Some(idx) = import_remap.get(i).copied().flatten() {
            dbg.push(crate::debug_info::FnDebugEntry::unlocated(idx, *name));
        }
    }
    for (k, n) in runtime::rt_fn_names().iter().enumerate() {
        dbg.push(crate::debug_info::FnDebugEntry::unlocated(
            actual_import_count + k as u32,
            *n,
        ));
    }
    for (proto, fd) in ordered.iter().enumerate() {
        let info = &infos[proto];
        let name = if async_set.contains(&fd.name) {
            format!("{}#resume", fd.name)
        } else {
            fd.name.clone()
        };
        dbg.push(crate::debug_info::FnDebugEntry {
            index: user_fn_base + proto as u32,
            name,
            file: info.source_file.clone(),
            line: info.source_line,
        });
    }
    // Scheduler helpers, in `emit_scheduler_functions` order.
    for (k, n) in [
        "sched_ready_push",
        "sched_ready_pop",
        "sched_spawn",
        "sched_complete",
        "sched_fail",
        "sched_sleep",
        "sched_notify_waiter",
        "sched_poll",
        "sched_start_async",
        "sched_resume_task",
        "sched_task_result",
        "sched_await",
        "sched_drive_closure",
        "sched_completed_pop",
    ]
    .iter()
    .enumerate()
    {
        dbg.push(crate::debug_info::FnDebugEntry::unlocated(
            sb + k as u32,
            *n,
        ));
    }
    for (i, c) in async_closures
        .iter()
        .chain(sync_closures.iter())
        .enumerate()
    {
        let name = if c.is_async {
            format!("{}#resume", c.info.name)
        } else {
            c.info.name.clone()
        };
        dbg.push(crate::debug_info::FnDebugEntry {
            index: closure_base + i as u32,
            name,
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
            ownership_sites: ownership_sites.into_inner(),
        },
    );

    if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
        for (proto, fd) in ordered.iter().enumerate() {
            eprintln!(
                "[async-engine] func {} = {} ({})",
                user_fn_base + proto as u32,
                fd.name,
                if async_set.contains(&fd.name) {
                    "async"
                } else {
                    "sync"
                }
            );
        }
    }
    let bytes = module.finish();
    // Soundness gate: never hand back a module that doesn't validate. Shapes
    // the engine can't yet lower correctly (notably **async closures** —
    // closures that await/fork are compiled as sync funcs and call an async
    // resume fn, producing a type-mismatched module) would otherwise emit
    // invalid wasm that only fails at instantiation. Validate here and fall
    // back to the existing path instead. (A3.0 lifts this for async closures.)
    if let Err(e) = wasmparser::validate(&bytes) {
        if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
            eprintln!("[async-engine] soundness gate rejected module: {e}");
            let _ = std::fs::write("/tmp/fai_invalid.wasm", &bytes);
            eprintln!("[async-engine] dumped invalid module to /tmp/fai_invalid.wasm");
        }
        return None;
    }
    Some(bytes)
}
