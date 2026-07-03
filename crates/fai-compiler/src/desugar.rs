//! AST desugarings applied right after native→serde conversion, so the
//! checker and the wasm backends all see the same rewritten form.
//!
//! Currently one rewrite lives here: **tail/return-position `from_dict`**.
//! `from_dict(d)` has no expression-level lowering — the backends expand
//! the statement shape `let x T = from_dict(d)` using the binding's type
//! annotation. A `from_dict` call in tail or `return` position type-checks
//! (the function's `@return` supplies the target type) but used to fall
//! through codegen to `UnknownIdentifier("from_dict")`, misattributed to
//! whatever unrelated `from_dict` an AST walk found first (ISSUES.md).
//! Rewriting
//!
//! ```text
//! def make
//!     @return HttpRequest
//! do
//!     from_dict(d)              # or: return from_dict(d)
//! end
//! ```
//!
//! into
//!
//! ```text
//!     let __from_dict_tail0 HttpRequest = from_dict(d)
//!     __from_dict_tail0         # or: return __from_dict_tail0
//! ```
//!
//! routes both positions through the one well-tested expansion in both the
//! sync and async lowerings. Only functions whose declared return type is
//! a single plain named type participate (no arrays, optionals, or type
//! parameters — those shapes keep their dedicated codegen diagnostic).

use crate::ast::*;

/// Synthesized binding names are numbered program-wide so two rewrites in
/// one scope (e.g. sibling `return from_dict(...)` arms) can't collide.
pub fn desugar_program(program: &mut Program) {
    let mut counter = 0u32;
    for stmt in &mut program.statements {
        desugar_statement(stmt, &mut counter);
    }
}

fn desugar_statement(stmt: &mut Statement, counter: &mut u32) {
    match stmt {
        Statement::FunctionDeclaration(fd) => desugar_function(fd, counter),
        Statement::TestDeclaration(td) => {
            for case in &mut td.cases {
                for s in &mut case.body {
                    desugar_statement(s, counter);
                }
            }
        }
        _ => {}
    }
}

fn desugar_function(fd: &mut FunctionDeclaration, counter: &mut u32) {
    // Local `def`s nested in the body get their own rewrite against their
    // own return type; closures (`do ... end`) declare no `@return`, so
    // there is nothing to desugar against for them.
    let ret = plain_named_return(fd);
    rewrite_body(&mut fd.body, ret.as_ref(), true, counter);
}

/// The function's declared return type when it is a single plain named
/// type — the only shape the `let x T = from_dict(...)` expansion accepts.
fn plain_named_return(fd: &FunctionDeclaration) -> Option<TypeNode> {
    if fd.return_types.len() != 1 {
        return None;
    }
    let node = &fd.return_types[0].type_node;
    let is_plain = node.name.is_some()
        && !node.is_array
        && !node.is_optional
        && !node.is_type_parameter.unwrap_or(false)
        && node.function_params.is_none();
    is_plain.then(|| node.clone())
}

/// Is `expr` a bare `from_dict(<one arg>)` call?
fn from_dict_call(expr: &Expression) -> bool {
    let Expression::CallExpression(ce) = expr else {
        return false;
    };
    let Expression::IdentifierExpression(id) = &*ce.callee else {
        return false;
    };
    id.name == "from_dict" && ce.args.len() == 1
}

/// Build the `let __from_dict_tailN T = <call>` statement plus the
/// replacement identifier expression, both located at the call site.
fn binding_for(call: Expression, ret: &TypeNode, counter: &mut u32) -> (Statement, Expression) {
    let location = match &call {
        Expression::CallExpression(ce) => ce.location.clone(),
        _ => unreachable!("binding_for only receives from_dict calls"),
    };
    let name = format!("__from_dict_tail{}", *counter);
    *counter += 1;
    let let_stmt = Statement::LetStatement(LetStatement {
        bindings: vec![BindingDeclaration {
            name: name.clone(),
            type_name: Some(ret.clone()),
        }],
        value: call,
        is_private: Some(false),
        is_shared: Some(false),
        location: location.clone(),
    });
    let ident = Expression::IdentifierExpression(IdentifierExpression { name, location });
    (let_stmt, ident)
}

/// Rewrite one statement list. `is_tail` means the list's last statement
/// is in value position for the enclosing function (so a trailing bare
/// `from_dict(...)` expression is that function's return value). `return`
/// statements are rewritten regardless of position. `ret` is `None` when
/// the enclosing function's return type can't anchor the rewrite — then
/// only recursion into nested declarations happens.
fn rewrite_body(
    body: &mut Vec<Statement>,
    ret: Option<&TypeNode>,
    is_tail: bool,
    counter: &mut u32,
) {
    let last = body.len().saturating_sub(1);
    let mut out: Vec<Statement> = Vec::with_capacity(body.len());
    for (i, mut stmt) in std::mem::take(body).into_iter().enumerate() {
        let tail_here = is_tail && i == last;
        match &mut stmt {
            Statement::ExpressionStatement(es) => {
                if tail_here {
                    if let Some(ret) = ret {
                        if from_dict_call(&es.expression) {
                            let call = std::mem::replace(
                                &mut es.expression,
                                Expression::NullExpression(NullExpression {
                                    location: es.location.clone(),
                                }),
                            );
                            let (let_stmt, ident) = binding_for(call, ret, counter);
                            es.expression = ident;
                            out.push(let_stmt);
                        }
                    }
                }
            }
            Statement::ReturnStatement(rs) => {
                if let (Some(ret), Some(value)) = (ret, rs.value.as_mut()) {
                    if from_dict_call(value) {
                        let call = std::mem::replace(
                            value,
                            Expression::NullExpression(NullExpression {
                                location: rs.location.clone(),
                            }),
                        );
                        let (let_stmt, ident) = binding_for(call, ret, counter);
                        *value = ident;
                        out.push(let_stmt);
                    }
                }
            }
            Statement::IfStatement(is) => {
                for branch in &mut is.branches {
                    rewrite_body(&mut branch.body, ret, tail_here, counter);
                }
                if let Some(else_branch) = &mut is.else_branch {
                    rewrite_body(else_branch, ret, tail_here, counter);
                }
            }
            Statement::CaseStatement(cs) => {
                for branch in &mut cs.when_branches {
                    rewrite_body(&mut branch.body, ret, tail_here, counter);
                }
                if let Some(default_branch) = &mut cs.default_branch {
                    rewrite_body(default_branch, ret, tail_here, counter);
                }
            }
            Statement::TryStatement(ts) => {
                rewrite_body(&mut ts.try_body, ret, tail_here, counter);
                rewrite_body(&mut ts.catch_body, ret, tail_here, counter);
                if let Some(finally_body) = &mut ts.finally_body {
                    rewrite_body(finally_body, ret, false, counter);
                }
            }
            Statement::ForStatement(fs) => rewrite_body(&mut fs.body, ret, false, counter),
            Statement::WhileStatement(ws) => rewrite_body(&mut ws.body, ret, false, counter),
            Statement::FunctionDeclaration(fd) => desugar_function(fd, counter),
            _ => {}
        }
        out.push(stmt);
    }
    *body = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare(source: &str) -> Program {
        let native = fai_parser::parse(source).expect("parse");
        crate::native_bridge::convert_program(&native)
    }

    fn fn_body<'a>(program: &'a Program, name: &str) -> &'a [Statement] {
        program
            .statements
            .iter()
            .find_map(|s| match s {
                Statement::FunctionDeclaration(fd) if fd.name == name => Some(&fd.body[..]),
                _ => None,
            })
            .expect("function not found")
    }

    #[test]
    fn tail_from_dict_becomes_typed_binding() {
        // convert_program runs the desugar, so the tail call is already
        // rewritten into `let __from_dict_tail0 T = from_dict(d)` + ident.
        let program = prepare(
            "# Makes a person.\n\
             def make\n    @return Person\ndo\n    let d = {}\n    from_dict(d)\nend\n",
        );
        let body = fn_body(&program, "make");
        assert_eq!(body.len(), 3, "let d, synthesized let, tail ident");
        let Statement::LetStatement(ls) = &body[1] else {
            panic!("expected synthesized let, got {:?}", body[1]);
        };
        assert_eq!(ls.bindings[0].name, "__from_dict_tail0");
        assert_eq!(
            ls.bindings[0].type_name.as_ref().unwrap().name.as_deref(),
            Some("Person")
        );
        let Statement::ExpressionStatement(es) = &body[2] else {
            panic!("expected tail expression, got {:?}", body[2]);
        };
        let Expression::IdentifierExpression(id) = &es.expression else {
            panic!("expected identifier tail, got {:?}", es.expression);
        };
        assert_eq!(id.name, "__from_dict_tail0");
    }

    #[test]
    fn return_from_dict_becomes_typed_binding() {
        let program = prepare(
            "# Makes a person.\n\
             def make\n    @param flag Bool\n    @return Person\ndo\n    if flag\n        return from_dict({})\n    end\n    from_dict({})\nend\n",
        );
        let body = fn_body(&program, "make");
        // if-branch: [let, return ident]; tail: [let, ident]
        let Statement::IfStatement(is) = &body[0] else {
            panic!("expected if, got {:?}", body[0]);
        };
        let branch = &is.branches[0].body;
        assert!(matches!(branch[0], Statement::LetStatement(_)));
        let Statement::ReturnStatement(rs) = &branch[1] else {
            panic!("expected return, got {:?}", branch[1]);
        };
        assert!(matches!(
            rs.value.as_ref().unwrap(),
            Expression::IdentifierExpression(_)
        ));
        // Distinct synthesized names for the two rewrites.
        assert!(matches!(&body[1], Statement::LetStatement(ls)
            if ls.bindings[0].name != "__from_dict_tail0"));
    }

    #[test]
    fn non_tail_and_unnamed_returns_untouched() {
        // Mid-body from_dict statement (not tail) and an optional return
        // type both stay as-is — the backend diagnostic covers them.
        let program = prepare(
            "# Maybe makes a person.\n\
             def maybe\n    @return Person?\ndo\n    from_dict({})\nend\n",
        );
        let body = fn_body(&program, "maybe");
        assert_eq!(body.len(), 1);
        assert!(matches!(&body[0], Statement::ExpressionStatement(es)
            if from_dict_call(&es.expression)));
    }
}
