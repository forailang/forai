//! Plan 133 phase 5 — public-endpoint × secrets lint (bridge to plan 132).
//!
//! A `@auth public` `remote def` is callable by anyone on the network.
//! If its call graph reaches a secrets API (`secrets.get`/`reveal`/
//! `resolveTemplate`/`revealOr`), an unauthenticated caller can trigger
//! secret-using egress — almost always a mistake. We emit a check-time
//! WARNING (v1; upgradeable to an error) naming the endpoint and the
//! reached secret call, so it surfaces without blocking the build.
//!
//! The analysis is a plain intra-program call-graph reachability walk
//! over every function body the checker saw (entry + modules): build a
//! name → direct-callees map, then DFS from each public remote def.

use std::collections::{HashMap, HashSet};

use fai_compiler::ast::*;

use super::Checker;

/// Secret entry points, matched both as `secrets.<method>` member calls
/// and as the bare builtin names the checker resolves them to.
const SECRET_MEMBER_METHODS: &[&str] = &["get", "reveal", "resolveTemplate", "revealOr"];
const SECRET_BUILTINS: &[&str] =
    &["secretsGet", "secretsReveal", "secretsResolveTemplate", "secretsRevealOr"];

/// Gather every top-level function declaration across the given
/// statement groups (entry + modules). Nested functions don't define
/// RPC endpoints, so top-level is sufficient for the call graph roots;
/// callee names still resolve across the flat function namespace.
pub(super) fn collect_fn_decls<'a>(groups: &[&'a [Statement]]) -> Vec<&'a FunctionDeclaration> {
    let mut out = Vec::new();
    for group in groups {
        for stmt in *group {
            if let Statement::FunctionDeclaration(fd) = stmt {
                out.push(fd);
            }
        }
    }
    out
}

/// One function's lint-relevant summary.
struct FnInfo {
    is_public_remote: bool,
    /// Direct callee function names (bare identifiers).
    callees: Vec<String>,
    /// The secret call spelled in THIS body, if any (for the message).
    direct_secret_call: Option<String>,
}

impl Checker {
    /// Run the public-endpoint × secrets lint over all collected function
    /// declarations and push warnings. Call after body checking.
    pub(super) fn lint_public_endpoints_reaching_secrets(&mut self, fn_decls: &[&FunctionDeclaration]) {
        let mut infos: HashMap<String, FnInfo> = HashMap::new();
        for fd in fn_decls {
            let mut callees = Vec::new();
            let mut direct_secret_call = None;
            for stmt in &fd.body {
                scan_stmt(stmt, &mut callees, &mut direct_secret_call);
            }
            let is_public_remote = fd.is_remote
                && fd
                    .auth_policy
                    .as_ref()
                    .is_some_and(|a| a.kind == "public");
            infos.insert(
                fd.name.clone(),
                FnInfo {
                    is_public_remote,
                    callees,
                    direct_secret_call,
                },
            );
        }

        for fd in fn_decls {
            let Some(info) = infos.get(&fd.name) else {
                continue;
            };
            if !info.is_public_remote {
                continue;
            }
            // DFS the call graph for the nearest secret call.
            let mut visited: HashSet<&str> = HashSet::new();
            let mut stack: Vec<&str> = vec![fd.name.as_str()];
            let mut found: Option<String> = None;
            while let Some(name) = stack.pop() {
                if !visited.insert(name) {
                    continue;
                }
                let Some(node) = infos.get(name) else {
                    continue;
                };
                if let Some(secret) = &node.direct_secret_call {
                    found = Some(secret.clone());
                    break;
                }
                for callee in &node.callees {
                    if !visited.contains(callee.as_str()) {
                        stack.push(callee.as_str());
                    }
                }
            }
            if let Some(secret) = found {
                self.warnings.push(format!(
                    "warning: `@auth public` remote def '{}' can reach {} — an \
                     unauthenticated caller can trigger secret-using egress. \
                     Use `@auth session` unless this endpoint is meant to be \
                     open. ({}:{}:{})",
                    fd.name,
                    secret,
                    self.current_file.as_deref().unwrap_or("<unknown>"),
                    fd.location.line,
                    fd.location.column,
                ));
            }
        }
    }
}

/// Record direct callee names and the first secret call spelled in a
/// statement subtree.
fn scan_stmt(stmt: &Statement, callees: &mut Vec<String>, secret: &mut Option<String>) {
    match stmt {
        Statement::ExpressionStatement(es) => scan_expr(&es.expression, callees, secret),
        Statement::LetStatement(ls) => scan_expr(&ls.value, callees, secret),
        Statement::VarStatement(vs) => scan_expr(&vs.value, callees, secret),
        Statement::AssignmentStatement(as_) => scan_expr(&as_.value, callees, secret),
        Statement::ThrowStatement(ts) => scan_expr(&ts.expression, callees, secret),
        Statement::NowaitStatement(ns) => scan_expr(&ns.expression, callees, secret),
        Statement::ReturnStatement(rs) => {
            if let Some(v) = &rs.value {
                scan_expr(v, callees, secret);
            }
        }
        Statement::IfStatement(is) => {
            for branch in &is.branches {
                scan_expr(&branch.condition, callees, secret);
                for s in &branch.body {
                    scan_stmt(s, callees, secret);
                }
            }
            if let Some(else_branch) = &is.else_branch {
                for s in else_branch {
                    scan_stmt(s, callees, secret);
                }
            }
        }
        Statement::ForStatement(fs) => {
            scan_expr(&fs.items, callees, secret);
            for s in &fs.body {
                scan_stmt(s, callees, secret);
            }
        }
        Statement::WhileStatement(ws) => {
            scan_expr(&ws.condition, callees, secret);
            for s in &ws.body {
                scan_stmt(s, callees, secret);
            }
        }
        Statement::TryStatement(ts) => {
            for s in &ts.try_body {
                scan_stmt(s, callees, secret);
            }
            for s in &ts.catch_body {
                scan_stmt(s, callees, secret);
            }
            if let Some(finally) = &ts.finally_body {
                for s in finally {
                    scan_stmt(s, callees, secret);
                }
            }
        }
        Statement::CaseStatement(cs) => {
            scan_expr(&cs.value, callees, secret);
            for branch in &cs.when_branches {
                scan_expr(&branch.match_expr, callees, secret);
                for s in &branch.body {
                    scan_stmt(s, callees, secret);
                }
            }
            if let Some(default) = &cs.default_branch {
                for s in default {
                    scan_stmt(s, callees, secret);
                }
            }
        }
        _ => {}
    }
}

fn scan_expr(expr: &Expression, callees: &mut Vec<String>, secret: &mut Option<String>) {
    match expr {
        Expression::CallExpression(ce) => {
            match ce.callee.as_ref() {
                // `secrets.get(...)` — member call on the `secrets` module.
                Expression::MemberExpression(me) => {
                    if let Expression::IdentifierExpression(obj) = me.object.as_ref() {
                        if obj.name == "secrets" && SECRET_MEMBER_METHODS.contains(&me.property.as_str())
                        {
                            secret.get_or_insert(format!("secrets.{}", me.property));
                        }
                    }
                    // The receiver may itself be a call (UFCS chains).
                    scan_expr(&me.object, callees, secret);
                }
                // Bare call: a user function, or the builtin the checker
                // lowers `secrets.get` to.
                Expression::IdentifierExpression(id) => {
                    if let Some(pos) = SECRET_BUILTINS.iter().position(|b| *b == id.name) {
                        secret.get_or_insert(format!("secrets.{}", SECRET_MEMBER_METHODS[pos]));
                    } else {
                        callees.push(id.name.clone());
                    }
                }
                other => scan_expr(other, callees, secret),
            }
            for arg in &ce.args {
                scan_expr(&arg.value, callees, secret);
            }
        }
        Expression::MemberExpression(me) => scan_expr(&me.object, callees, secret),
        Expression::BinaryExpression(be) => {
            scan_expr(&be.left, callees, secret);
            scan_expr(&be.right, callees, secret);
        }
        Expression::UnaryExpression(ue) => scan_expr(&ue.expression, callees, secret),
        Expression::OptionalCheckExpression(oce) => scan_expr(&oce.expression, callees, secret),
        Expression::ForceUnwrapExpression(fue) => scan_expr(&fue.expression, callees, secret),
        Expression::IndexExpression(ie) => {
            scan_expr(&ie.object, callees, secret);
            scan_expr(&ie.index, callees, secret);
        }
        Expression::ArrayExpression(ae) => {
            for e in &ae.items {
                scan_expr(e, callees, secret);
            }
        }
        Expression::TupleExpression(te) => {
            for e in &te.items {
                scan_expr(e, callees, secret);
            }
        }
        Expression::DictionaryExpression(de) => {
            for entry in &de.entries {
                scan_expr(&entry.value, callees, secret);
            }
        }
        Expression::TemplateStringExpression(ts) => {
            for part in &ts.parts {
                if let TemplateStringPart::Expression { expression } = part {
                    scan_expr(expression, callees, secret);
                }
            }
        }
        _ => {}
    }
}
