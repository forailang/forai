//! Plan 134 phase 3 — session-cookie safety lint.
//!
//! A raw `let c Cookie = from_dict({ name: 'session...', value: token })`
//! that omits `httpOnly`/`sameSite` ships a session credential that JS can
//! read (XSS token theft) and that rides cross-site requests (CSRF). The
//! safe path is `Forui.rpc.sessionCookie`, which sets HttpOnly + Secure +
//! SameSite=Lax by default. We can't type-track "this string is a session
//! token", so this is a best-effort name heuristic: a Cookie literal whose
//! `name` looks session-ish (`session`/`token`, but not the intentionally
//! JS-readable `csrf`) and that lacks `httpOnly: true` or a `sameSite`.
//! Check-time WARNING (v1), never blocks the build.

use fai_compiler::ast::*;

use super::Checker;

impl Checker {
    /// Warn on raw session-shaped `Cookie` literals missing HttpOnly/SameSite.
    /// Call after body checking, over entry + module statement groups.
    pub(super) fn lint_session_cookie_attrs(&mut self, groups: &[&[Statement]]) {
        for group in groups {
            for stmt in *group {
                self.scan_stmt_for_cookie(stmt);
            }
        }
    }

    fn scan_stmt_for_cookie(&mut self, stmt: &Statement) {
        match stmt {
            Statement::FunctionDeclaration(fd) => {
                for s in &fd.body {
                    self.scan_stmt_for_cookie(s);
                }
            }
            Statement::LetStatement(ls) => {
                self.check_cookie_binding(&ls.bindings, &ls.value);
            }
            Statement::VarStatement(vs) => {
                self.check_cookie_binding(&vs.bindings, &vs.value);
            }
            Statement::IfStatement(is) => {
                for b in &is.branches {
                    for s in &b.body {
                        self.scan_stmt_for_cookie(s);
                    }
                }
                if let Some(eb) = &is.else_branch {
                    for s in eb {
                        self.scan_stmt_for_cookie(s);
                    }
                }
            }
            Statement::ForStatement(fs) => {
                for s in &fs.body {
                    self.scan_stmt_for_cookie(s);
                }
            }
            Statement::WhileStatement(ws) => {
                for s in &ws.body {
                    self.scan_stmt_for_cookie(s);
                }
            }
            Statement::TryStatement(ts) => {
                for s in &ts.try_body {
                    self.scan_stmt_for_cookie(s);
                }
                for s in &ts.catch_body {
                    self.scan_stmt_for_cookie(s);
                }
                if let Some(fb) = &ts.finally_body {
                    for s in fb {
                        self.scan_stmt_for_cookie(s);
                    }
                }
            }
            _ => {}
        }
    }

    /// A `let/var <name> Cookie = from_dict({ ... })` with a session-shaped
    /// `name` and no `httpOnly: true` (or no `sameSite`) draws a warning.
    fn check_cookie_binding(&mut self, bindings: &[BindingDeclaration], value: &Expression) {
        let is_cookie = bindings.iter().any(|b| {
            b.type_name
                .as_ref()
                .and_then(|t| t.name.as_deref())
                .is_some_and(|n| n == "Cookie")
        });
        if !is_cookie {
            return;
        }
        let Some(dict) = from_dict_literal(value) else {
            return;
        };
        let mut name_val: Option<&str> = None;
        let mut http_only_true = false;
        let mut has_same_site = false;
        for e in &dict.entries {
            match e.key.as_str() {
                "name" => {
                    if let Expression::StringExpression(se) = &e.value {
                        name_val = Some(se.value.as_str());
                    }
                }
                "httpOnly" => {
                    if let Expression::BooleanExpression(be) = &e.value {
                        http_only_true = be.value;
                    }
                }
                "sameSite" => has_same_site = true,
                _ => {}
            }
        }
        let Some(name) = name_val else {
            return;
        };
        let lname = name.to_ascii_lowercase();
        let session_shaped =
            (lname.contains("session") || lname.contains("token")) && !lname.contains("csrf");
        if session_shaped && (!http_only_true || !has_same_site) {
            self.warnings.push(format!(
                "warning: session-shaped cookie '{}' is built as a raw dict \
                 missing {} — a JS-readable / cross-site session token. Use \
                 `Forui.rpc.sessionCookie(name, value)` (HttpOnly + Secure + \
                 SameSite=Lax by default). ({}:{}:{})",
                name,
                if !http_only_true {
                    "httpOnly: true"
                } else {
                    "sameSite"
                },
                self.current_file.as_deref().unwrap_or("<unknown>"),
                dict.location.line,
                dict.location.column,
            ));
        }
    }
}

/// `from_dict({ ... })` -> the dictionary literal argument, else None.
fn from_dict_literal(value: &Expression) -> Option<&DictionaryExpression> {
    let Expression::CallExpression(call) = value else {
        return None;
    };
    let Expression::IdentifierExpression(id) = call.callee.as_ref() else {
        return None;
    };
    if id.name != "from_dict" || call.args.len() != 1 {
        return None;
    }
    match &call.args[0].value {
        Expression::DictionaryExpression(d) => Some(d),
        _ => None,
    }
}
