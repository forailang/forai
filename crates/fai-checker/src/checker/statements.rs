//! Statement-level type checking.

use fai_compiler::ast::*;

use super::program::install_failed_binding_placeholders;
use super::Checker;
use crate::environment::Environment;
use crate::error::CheckError;
use crate::types::*;

impl Checker {
    /// Type-check one statement, attaching the statement's own source
    /// location to any error that doesn't already carry one. Expression
    /// checks attach their (more precise) location first, so this is the
    /// statement-granularity fallback that keeps body errors off the
    /// enclosing `def` line (plan 130 A1).
    pub(super) fn check_statement(
        &mut self,
        stmt: &Statement,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        self.check_statement_unlocated(stmt, env)
            .map_err(|e| self.attach_location(e, super::statement_location(stmt)))
    }

    fn check_statement_unlocated(
        &mut self,
        stmt: &Statement,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        match stmt {
            Statement::ExpressionStatement(es) => self.check_expression(&es.expression, env),
            Statement::IfStatement(is) => self.check_if_statement(is, env),
            Statement::CaseStatement(cs) => self.check_case_statement(cs, env),
            Statement::TryStatement(ts) => self.check_try_statement(ts, env),
            Statement::ThrowStatement(ts) => {
                self.check_expression(&ts.expression, env)?;
                Ok(Type::Never)
            }
            Statement::NowaitStatement(nw) => {
                // The forked call's target may not take `mutable` params (a
                // detached task would outlive the caller's binding). The flag
                // is consumed by the call-check on the outermost call.
                self.in_nowait_fork = true;
                let r = self.check_expression(&nw.expression, env);
                self.in_nowait_fork = false;
                r?;
                Ok(Type::Void)
            }
            Statement::ForStatement(fs) => self.check_for_statement(fs, env),
            Statement::WhileStatement(ws) => self.check_while_statement(ws, env),
            Statement::BreakStatement(_) => {
                if self.loop_depth == 0 {
                    return Err(CheckError::new("break can only be used inside a loop"));
                }
                Ok(Type::Never)
            }
            Statement::ContinueStatement(_) => {
                if self.loop_depth == 0 {
                    return Err(CheckError::new("continue can only be used inside a loop"));
                }
                Ok(Type::Never)
            }
            Statement::ReturnStatement(rs) => {
                // Type-check the returned value if present. A fuller
                // pass would verify it matches the enclosing function's
                // declared @return, but function scope isn't threaded
                // through check_statement today — lean on the tail-
                // expression return-type check to catch mismatches on
                // the trailing path.
                if let Some(expr) = &rs.value {
                    self.check_expression(expr, env)?;
                }
                Ok(Type::Never)
            }
            Statement::UseStatement(_) => Ok(Type::Void),
            Statement::LetStatement(ls) => {
                self.check_binding_statement(&ls.bindings, &ls.value, env, false, "let")?;
                Ok(Type::Void)
            }
            Statement::VarStatement(vs) => {
                self.check_binding_statement(&vs.bindings, &vs.value, env, true, "var")?;
                Ok(Type::Void)
            }
            Statement::AssignmentStatement(a) => {
                self.check_assignment_stmt(a, env)?;
                Ok(Type::Void)
            }
            Statement::FunctionDeclaration(fd) => {
                // Local function — define and check
                let fn_type = self.function_type_from_decl(fd)?;
                let _ = env.define(&fd.name, fn_type, false);
                self.check_function(fd, env)?;
                Ok(Type::Void)
            }
            Statement::TypeDeclaration(_)
            | Statement::EnumDeclaration(_)
            | Statement::FunctionTypeDefDeclaration(_)
            | Statement::ExternBlockDeclaration(_) => Ok(Type::Void),
            Statement::TestDeclaration(td) => {
                self.check_test_declaration(td, env)?;
                Ok(Type::Void)
            }
        }
    }

    pub(super) fn check_block(
        &mut self,
        statements: &[Statement],
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        let mut last_type = Type::Void;
        for stmt in statements {
            match self.check_statement(stmt, env) {
                Ok(t) => last_type = t,
                Err(e) => {
                    // Same idea as check_top_level_statements: keep checking
                    // the rest of the block so one bad expression doesn't
                    // hide the rest. Fall back to Type::Unknown so the
                    // enclosing context degrades silently rather than
                    // exploding with cascade errors.
                    install_failed_binding_placeholders(stmt, env);
                    self.collected_errors.push(e);
                    last_type = Type::Unknown;
                }
            }
        }
        Ok(last_type)
    }

    pub(super) fn check_function(
        &mut self,
        fd: &FunctionDeclaration,
        env: &mut Environment,
    ) -> Result<(), CheckError> {
        let is_synthetic = fd.name.starts_with('<');
        let is_main = fd.name == "main";

        // Doc enforcement: named functions (not synthetic, not main) must have a doc comment.
        // Matches language.md's "Doc comment required above `def`" rule.
        //
        // The error includes a paste-ready example so agents (who
        // commonly invent Python docstrings, /// rustdoc, or JSDoc)
        // see the exact `# Description.` shape forai expects without
        // a trip to the docs.
        if !is_synthetic && !is_main {
            if fd.doc_comment.is_none() {
                let err = CheckError::new(format!(
                    "Function '{}' is missing a required doc comment. \
                     Add a `# Description.` line directly above the `def`:\n\n  \
                     # What this function does.\n  \
                     def {}\n  \
                     ...\n\n\
                     Every named `def`, `remote def`, and `test` block needs one. \
                     `main` is the only exemption. Note: a blank line between \
                     a comment and its `def` breaks the attachment — the doc \
                     comment must sit directly above the declaration.",
                    fd.name, fd.name
                ));
                self.collected_errors
                    .push(self.attach_location(err, &fd.location));
            }
        }

        // @auth enforcement (plan 133). Default-deny: every remote def
        // must declare its policy, so a new endpoint cannot ship publicly
        // callable by omission. `public` is the only way to be open, and
        // it is greppable.
        if fd.is_remote && !is_synthetic {
            match &fd.auth_policy {
                None => {
                    let err = CheckError::new(format!(
                        "remote def '{}' must declare @auth. Every RPC endpoint \
                         declares its auth policy in the contract:\n\n  \
                         @auth session            # authenticated caller required\n  \
                         @auth public             # explicitly open to anyone\n  \
                         @auth session, role: 'admin'  # session + named authorizer\n\n\
                         Add the line after @param and before @return.",
                        fd.name
                    ));
                    self.collected_errors
                        .push(self.attach_location(err, &fd.location));
                }
                Some(auth) => {
                    match auth.kind.as_str() {
                        "public" => {
                            if auth.authorizer.is_some() {
                                let err = CheckError::new(format!(
                                    "@auth public on '{}' cannot take an authorizer — \
                                     public endpoints run no auth checks. Use \
                                     `@auth session, {}: '{}'` to require one.",
                                    fd.name,
                                    auth.label.as_deref().unwrap_or("role"),
                                    auth.authorizer.as_deref().unwrap_or(""),
                                ));
                                self.collected_errors
                                    .push(self.attach_location(err, &auth.location));
                            }
                        }
                        "session" => {}
                        other => {
                            let err = CheckError::new(format!(
                                "Unknown @auth policy '{}' on '{}'. Valid policies: \
                                 `public`, `session`, or `session, <label>: '<authorizer>'`.",
                                other, fd.name
                            ));
                            self.collected_errors
                                .push(self.attach_location(err, &auth.location));
                        }
                    }
                }
            }
        } else if fd.auth_policy.is_some() && !fd.is_remote {
            let err = CheckError::new(format!(
                "@auth is only valid on `remote def` — '{}' is not remote. \
                 Auth policies gate the RPC dispatch boundary; a local \
                 function has no caller to authenticate.",
                fd.name
            ));
            self.collected_errors
                .push(self.attach_location(err, &fd.location));
        }

        // Abstract functions (no body) are interface declarations —
        // validate param types but skip body/return checking.
        if fd.is_abstract {
            for param in &fd.params {
                let _ = self.resolve_type_node(&param.type_node)?;
            }
            for rd in &fd.return_types {
                let _ = self.resolve_type_node(&rd.type_node)?;
            }
            return Ok(());
        }

        env.push_scope();
        // Register @type T params as Unknown so they can be used as values
        // inside the body (e.g. `from_dict(T, row)`). The compiler will inject
        // the actual constructor as a hidden argument at each call site.
        for tp in &fd.type_params {
            let _ = env.define(&tp.name, Type::Unknown, false);
        }
        for param in &fd.params {
            let param_type = self.resolve_type_node(&param.type_node)?;
            env.define(&param.name, param_type, param.is_mutable)?;
        }

        let errors_before_body = self.collected_errors.len();
        let body_type = self.check_block(&fd.body, env)?;
        // Suppress the return-type check when the body already accumulated
        // errors: the body_type is Type::Unknown as a fallback, which would
        // otherwise produce a misleading "Function X returns Unknown"
        // message that obscures the real error inside the body.
        let body_had_errors = self.collected_errors.len() != errors_before_body;
        env.pop_scope();

        let return_types: Vec<Type> = fd
            .return_types
            .iter()
            .map(|rd| self.resolve_type_node(&rd.type_node))
            .collect::<Result<_, _>>()?;

        if return_types.len() == 1 {
            if !body_had_errors && !is_assignable(&body_type, &return_types[0]) {
                return Err(CheckError::new(format!(
                    "Function '{}' returns {} but expected {}",
                    fd.name,
                    describe_type(&body_type),
                    describe_type(&return_types[0])
                )));
            }
        } else if return_types.len() > 1 && !body_had_errors {
            match &body_type {
                Type::Tuple(items) => {
                    if items.len() != return_types.len() {
                        return Err(CheckError::new(format!(
                            "Function '{}' must return {} values, got {}",
                            fd.name,
                            return_types.len(),
                            items.len()
                        )));
                    }
                    for (i, (actual, expected)) in items.iter().zip(return_types.iter()).enumerate()
                    {
                        if !is_assignable(actual, expected) {
                            return Err(CheckError::new(format!(
                                "Function '{}' return value {} expects {}, got {}",
                                fd.name,
                                i + 1,
                                describe_type(expected),
                                describe_type(actual)
                            )));
                        }
                    }
                }
                _ => {
                    return Err(CheckError::new(format!(
                        "Function '{}' must return {}, got {}",
                        fd.name,
                        describe_type(&tuple_of(return_types)),
                        describe_type(&body_type)
                    )));
                }
            }
        }

        Ok(())
    }

    pub(super) fn check_binding_statement(
        &mut self,
        bindings: &[BindingDeclaration],
        value: &Expression,
        env: &mut Environment,
        mutable: bool,
        kind: &str,
    ) -> Result<Type, CheckError> {
        // If there is a single binding with an explicit type annotation and the
        // RHS is a plain call expression, pass the declared type as a hint so
        // that unresolved `@type T` params can be inferred from the LHS.
        let lhs_hint: Option<Type> = if bindings.len() == 1 {
            bindings[0]
                .type_name
                .as_ref()
                .and_then(|tn| self.resolve_type_node(tn).ok())
        } else {
            None
        };
        let value_type = if lhs_hint.is_some() {
            if let Expression::CallExpression(ce) = value {
                self.check_call_expression_with_hint(ce, env, lhs_hint)?
            } else {
                self.check_expression(value, env)?
            }
        } else {
            self.check_expression(value, env)?
        };

        if bindings.len() == 1 {
            if matches!(&value_type, Type::Tuple(_)) {
                return Err(CheckError::new(format!(
                    "Cannot assign multiple values {} to single {} '{}'",
                    describe_type(&value_type),
                    kind,
                    bindings[0].name
                )));
            }
            let declared_type = if let Some(tn) = &bindings[0].type_name {
                self.resolve_type_node(tn)?
            } else {
                value_type.clone()
            };
            let effective = self.refine_literal_type(value, &value_type, &declared_type);
            if !is_assignable(&effective, &declared_type)
                && !is_numeric_coercible(&effective, &declared_type)
            {
                return Err(CheckError::new(format!(
                    "Cannot assign {} to {} in {} '{}'",
                    describe_type(&value_type),
                    describe_type(&declared_type),
                    kind,
                    bindings[0].name
                )));
            }
            env.define(&bindings[0].name, declared_type.clone(), mutable)?;
            return Ok(declared_type);
        }

        // Multiple bindings — need tuple
        match &value_type {
            Type::Tuple(items) => {
                if items.len() != bindings.len() {
                    return Err(CheckError::new(format!(
                        "Expected {} values in {} binding, got {}",
                        bindings.len(),
                        kind,
                        items.len()
                    )));
                }
                for (i, binding) in bindings.iter().enumerate() {
                    let actual = &items[i];
                    let declared = if let Some(tn) = &binding.type_name {
                        self.resolve_type_node(tn)?
                    } else {
                        actual.clone()
                    };
                    let effective = self.refine_literal_type(value, actual, &declared);
                    if !is_assignable(&effective, &declared)
                        && !is_numeric_coercible(&effective, &declared)
                    {
                        return Err(CheckError::new(format!(
                            "Cannot assign {} to {} in {} '{}'",
                            describe_type(actual),
                            describe_type(&declared),
                            kind,
                            binding.name
                        )));
                    }
                    env.define(&binding.name, declared, mutable)?;
                }
                Ok(value_type)
            }
            Type::Unknown => {
                // Unknown type (e.g., from all()) — allow destructuring, each binding gets Unknown
                for binding in bindings.iter() {
                    env.define(&binding.name, Type::Unknown, mutable)?;
                }
                Ok(value_type)
            }
            _ => Err(CheckError::new(format!(
                "Multiple {} bindings require multiple values, got {}",
                kind,
                describe_type(&value_type)
            ))),
        }
    }

    pub(super) fn check_assignment_stmt(
        &mut self,
        a: &AssignmentStatement,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        match &a.target {
            AssignmentTarget::Variables { names } => self.check_assignment(names, &a.value, env),
            AssignmentTarget::Field { object } => {
                // Field assignment: obj.field = value
                // Check that the root variable is mutable (var)
                if let Some(root_name) = self.extract_root_name(object) {
                    let binding = env.get(&root_name)?;
                    if !binding.mutable {
                        return Err(CheckError::new(format!(
                            "Cannot mutate field on immutable binding '{}'. Use 'var' instead of 'let'",
                            root_name
                        )));
                    }
                }
                let _obj_type = self.check_expression(object, env)?;
                let value_type = self.check_expression(&a.value, env)?;
                Ok(value_type)
            }
            AssignmentTarget::Index { object } => {
                // Index assignment: obj[i] = value
                if let Some(root_name) = self.extract_root_name(object) {
                    let binding = env.get(&root_name)?;
                    if !binding.mutable {
                        return Err(CheckError::new(format!(
                            "Cannot mutate index on immutable binding '{}'. Use 'var' instead of 'let'",
                            root_name
                        )));
                    }
                }
                let _obj_type = self.check_expression(object, env)?;
                let value_type = self.check_expression(&a.value, env)?;
                Ok(value_type)
            }
        }
    }

    /// Extract the root variable name from a nested member/index expression.
    fn extract_root_name(&self, expr: &Expression) -> Option<String> {
        match expr {
            Expression::IdentifierExpression(id) => Some(id.name.clone()),
            Expression::MemberExpression(me) => self.extract_root_name(&me.object),
            Expression::IndexExpression(ie) => self.extract_root_name(&ie.object),
            _ => None,
        }
    }

    fn check_assignment(
        &mut self,
        names: &[String],
        value: &Expression,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        let value_type = self.check_expression(value, env)?;

        if names.len() == 1 {
            if matches!(&value_type, Type::Tuple(_)) {
                return Err(CheckError::new(format!(
                    "Cannot assign multiple values {} to single name '{}'",
                    describe_type(&value_type),
                    names[0]
                )));
            }
            let target = env.get(&names[0])?.ty.clone();
            let effective = self.refine_literal_type(value, &value_type, &target);
            env.assign(&names[0], &effective)?;
            return Ok(effective);
        }

        match &value_type {
            Type::Tuple(items) => {
                if items.len() != names.len() {
                    return Err(CheckError::new(format!(
                        "Expected {} values in assignment, got {}",
                        names.len(),
                        items.len()
                    )));
                }
                for (i, name) in names.iter().enumerate() {
                    env.assign(name, &items[i])?;
                }
                Ok(value_type)
            }
            _ => Err(CheckError::new(format!(
                "Multiple assignment requires multiple values, got {}",
                describe_type(&value_type)
            ))),
        }
    }

    fn check_if_statement(
        &mut self,
        is: &IfStatement,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        if is.else_branch.is_none() {
            // No else — type is Void
            for branch in &is.branches {
                let cond = self.check_expression(&branch.condition, env)?;
                if !same_type(&cond, &Type::Bool) {
                    return Err(CheckError::new(format!(
                        "If condition must be Bool, got {}",
                        describe_type(&cond)
                    )));
                }
                env.push_scope();
                self.check_block(&branch.body, env)?;
                env.pop_scope();
            }
            return Ok(Type::Void);
        }

        let mut branch_types = Vec::new();
        for branch in &is.branches {
            let cond = self.check_expression(&branch.condition, env)?;
            if !same_type(&cond, &Type::Bool) {
                return Err(CheckError::new(format!(
                    "If condition must be Bool, got {}",
                    describe_type(&cond)
                )));
            }
            env.push_scope();
            let bt = self.check_block(&branch.body, env)?;
            env.pop_scope();
            branch_types.push(bt);
        }
        if let Some(else_body) = &is.else_branch {
            env.push_scope();
            let et = self.check_block(else_body, env)?;
            env.pop_scope();
            branch_types.push(et);
        }

        self.ensure_consistent_branch_types(&branch_types, "if")
    }

    fn check_case_statement(
        &mut self,
        cs: &CaseStatement,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        let value_type = self.check_expression(&cs.value, env)?;

        // Secrets are opaque (plan 132): case dispatch on one would probe
        // its identity byte-by-byte, so it is rejected like ==/ordering.
        if matches!(value_type, Type::Secret) {
            return Err(CheckError::new(
                "Cannot use a Secret as a case value. Secrets are opaque \
                 handles; pass one to an egress position (e.g. an HTTP \
                 header) or use secrets.reveal(...) at a trusted sink",
            ));
        }

        if cs.default_branch.is_none() {
            for branch in &cs.when_branches {
                let match_type = self.check_expression(&branch.match_expr, env)?;
                if !same_type(&value_type, &match_type) {
                    return Err(CheckError::new(format!(
                        "Case match type {} does not match case value type {}",
                        describe_type(&match_type),
                        describe_type(&value_type)
                    )));
                }
                env.push_scope();
                self.check_block(&branch.body, env)?;
                env.pop_scope();
            }
            return Ok(Type::Void);
        }

        let mut branch_types = Vec::new();
        for branch in &cs.when_branches {
            let match_type = self.check_expression(&branch.match_expr, env)?;
            if !same_type(&value_type, &match_type) {
                return Err(CheckError::new(format!(
                    "Case match type {} does not match case value type {}",
                    describe_type(&match_type),
                    describe_type(&value_type)
                )));
            }
            env.push_scope();
            let bt = self.check_block(&branch.body, env)?;
            env.pop_scope();
            branch_types.push(bt);
        }
        if let Some(default_body) = &cs.default_branch {
            env.push_scope();
            let dt = self.check_block(default_body, env)?;
            env.pop_scope();
            branch_types.push(dt);
        }

        self.ensure_consistent_branch_types(&branch_types, "case")
    }

    fn check_try_statement(
        &mut self,
        ts: &TryStatement,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        env.push_scope();
        self.check_block(&ts.try_body, env)?;
        env.pop_scope();

        env.push_scope();
        env.define(&ts.catch_name, Type::Error, false)?;
        self.check_block(&ts.catch_body, env)?;
        env.pop_scope();

        if let Some(finally_body) = &ts.finally_body {
            env.push_scope();
            let ft = self.check_block(finally_body, env)?;
            env.pop_scope();
            return Ok(ft);
        }

        // Re-check for branch type consistency
        env.push_scope();
        let try_type = self.check_block(&ts.try_body, env)?;
        env.pop_scope();

        env.push_scope();
        env.define(&ts.catch_name, Type::Error, false)?;
        let catch_type = self.check_block(&ts.catch_body, env)?;
        env.pop_scope();

        self.ensure_consistent_branch_types(&[try_type, catch_type], "try/catch")
    }

    fn check_for_statement(
        &mut self,
        fs: &ForStatement,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        let items_type = self.check_expression(&fs.items, env)?;
        let item_type = match &items_type {
            Type::Array(inner) => (**inner).clone(),
            // Generic type parameters and Unknown: allow iteration, resolve at runtime
            Type::TypeParameter(_) | Type::Unknown => Type::Unknown,
            _ => {
                return Err(CheckError::new(format!(
                    "For loop requires an array, got {}",
                    describe_type(&items_type)
                )));
            }
        };

        env.push_scope();
        env.define(&fs.item_name, item_type, false)?;
        self.loop_depth += 1;
        let result = self.check_block(&fs.body, env);
        self.loop_depth -= 1;
        env.pop_scope();
        result?;
        Ok(Type::Void)
    }

    fn check_while_statement(
        &mut self,
        ws: &WhileStatement,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        let cond_type = self.check_expression(&ws.condition, env)?;
        if !matches!(cond_type, Type::Bool) {
            return Err(CheckError::new(format!(
                "While condition must be Bool, got {}",
                describe_type(&cond_type)
            )));
        }

        env.push_scope();
        self.loop_depth += 1;
        let result = self.check_block(&ws.body, env);
        self.loop_depth -= 1;
        env.pop_scope();
        result?;
        Ok(Type::Void)
    }

    pub(super) fn check_test_declaration(
        &mut self,
        td: &TestDeclaration,
        env: &mut Environment,
    ) -> Result<(), CheckError> {
        // Verify the suite target exists and is a function
        let suite_target = env.get(&td.name)?;
        if !matches!(&suite_target.ty, Type::Function(_)) {
            return Err(CheckError::new(format!(
                "Test suite '{}' must refer to a function",
                td.name
            )));
        }

        // Check setup in suite scope
        env.push_scope();
        for setup_stmt in &td.setup {
            self.check_statement(setup_stmt, env)?;
        }
        env.pop_scope();

        if let Some(before_all) = &td.before_all {
            env.push_scope();
            self.check_block(before_all, env)?;
            env.pop_scope();
        }
        if let Some(after_all) = &td.after_all {
            env.push_scope();
            self.check_block(after_all, env)?;
            env.pop_scope();
        }

        for case in &td.cases {
            env.push_scope();
            // Re-run setup for each case
            for setup_stmt in &td.setup {
                self.check_statement(setup_stmt, env)?;
            }
            if let Some(before_each) = &td.before_each {
                self.check_block(before_each, env)?;
            }
            self.check_block(&case.body, env)?;
            if let Some(after_each) = &td.after_each {
                self.check_block(after_each, env)?;
            }
            env.pop_scope();
        }

        Ok(())
    }

    fn ensure_consistent_branch_types(
        &self,
        types: &[Type],
        context: &str,
    ) -> Result<Type, CheckError> {
        let concrete: Vec<&Type> = types.iter().filter(|t| !matches!(t, Type::Never)).collect();
        if concrete.is_empty() {
            return Ok(Type::Never);
        }
        let mut unified = concrete[0].clone();
        for ty in &concrete[1..] {
            if let Some(u) = unify_branch_type(&unified, ty) {
                unified = u;
            } else {
                return Err(CheckError::new(format!(
                    "All {} branches must return the same type: got {} and {}",
                    context,
                    describe_type(&unified),
                    describe_type(ty)
                )));
            }
        }
        Ok(unified)
    }

    fn refine_literal_type(&self, expr: &Expression, actual: &Type, target: &Type) -> Type {
        // Empty array literal → refine to target array type
        if let Expression::ArrayExpression(ae) = expr {
            if ae.items.is_empty() {
                if let (Type::Array(inner), Type::Array(_)) = (actual, target) {
                    if same_type(inner, &Type::Unknown) {
                        return target.clone();
                    }
                }
            }
        }
        // Numeric literal with explicit target: allow `let x Int = 1.0`
        // (whole-valued Float literal narrows to Int exactly) but not
        // `let x Int = 1.23`. Int → Float is always safe and already
        // covered by `is_numeric_coercible`, but refining here keeps
        // the recorded expression type aligned with the annotation.
        if let Expression::NumberExpression(n) = expr {
            if n.is_float
                && matches!(target, Type::Int)
                && n.value.is_finite()
                && n.value == n.value.trunc()
            {
                return Type::Int;
            }
            if !n.is_float && matches!(target, Type::Float) {
                return Type::Float;
            }
        }
        actual.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc() -> SourceLocation {
        SourceLocation { line: 1, column: 1 }
    }

    fn ident(name: &str) -> Expression {
        Expression::IdentifierExpression(IdentifierExpression {
            name: name.to_string(),
            location: loc(),
        })
    }

    #[test]
    fn test_extract_root_name_identifier() {
        let c = Checker::new();
        let expr = ident("foo");
        assert_eq!(c.extract_root_name(&expr).as_deref(), Some("foo"));
    }

    #[test]
    fn test_extract_root_name_member() {
        let c = Checker::new();
        let expr = Expression::MemberExpression(MemberExpression {
            object: Box::new(ident("obj")),
            property: "field".to_string(),
            location: loc(),
        });
        assert_eq!(c.extract_root_name(&expr).as_deref(), Some("obj"));
    }

    #[test]
    fn test_extract_root_name_nested_member() {
        let c = Checker::new();
        let inner = Expression::MemberExpression(MemberExpression {
            object: Box::new(ident("root")),
            property: "mid".to_string(),
            location: loc(),
        });
        let outer = Expression::MemberExpression(MemberExpression {
            object: Box::new(inner),
            property: "leaf".to_string(),
            location: loc(),
        });
        assert_eq!(c.extract_root_name(&outer).as_deref(), Some("root"));
    }

    #[test]
    fn test_extract_root_name_index() {
        let c = Checker::new();
        let expr = Expression::IndexExpression(IndexExpression {
            object: Box::new(ident("arr")),
            index: Box::new(ident("i")),
            location: loc(),
        });
        assert_eq!(c.extract_root_name(&expr).as_deref(), Some("arr"));
    }

    #[test]
    fn test_extract_root_name_other_returns_none() {
        let c = Checker::new();
        let expr = Expression::BooleanExpression(BooleanExpression {
            value: true,
            location: loc(),
        });
        assert!(c.extract_root_name(&expr).is_none());
    }

    #[test]
    fn test_ensure_consistent_branch_types_all_never() {
        let c = Checker::new();
        let result = c
            .ensure_consistent_branch_types(&[Type::Never, Type::Never], "if")
            .unwrap();
        assert!(matches!(result, Type::Never));
    }

    #[test]
    fn test_ensure_consistent_branch_types_all_same() {
        let c = Checker::new();
        let result = c
            .ensure_consistent_branch_types(&[Type::Int, Type::Int], "if")
            .unwrap();
        assert!(matches!(result, Type::Int));
    }

    #[test]
    fn test_ensure_consistent_branch_types_with_never() {
        let c = Checker::new();
        let result = c
            .ensure_consistent_branch_types(&[Type::Int, Type::Never], "if")
            .unwrap();
        assert!(matches!(result, Type::Int));
    }

    #[test]
    fn test_ensure_consistent_branch_types_mismatch() {
        let c = Checker::new();
        let result = c.ensure_consistent_branch_types(&[Type::Int, Type::String], "if");
        assert!(result.is_err());
    }

    #[test]
    fn test_refine_literal_type_empty_array_refines_to_target() {
        let c = Checker::new();
        let expr = Expression::ArrayExpression(ArrayExpression {
            items: vec![],
            style: ArrayLiteralStyle::Inline,
            location: loc(),
        });
        let actual = array_of(Type::Unknown);
        let target = array_of(Type::Int);
        let result = c.refine_literal_type(&expr, &actual, &target);
        match result {
            Type::Array(inner) => assert!(matches!(*inner, Type::Int)),
            _ => panic!("expected Array"),
        }
    }

    #[test]
    fn test_refine_literal_type_nonempty_array_unchanged() {
        let c = Checker::new();
        let expr = Expression::ArrayExpression(ArrayExpression {
            items: vec![ident("x")],
            style: ArrayLiteralStyle::Inline,
            location: loc(),
        });
        let actual = array_of(Type::Int);
        let target = array_of(Type::Float);
        let result = c.refine_literal_type(&expr, &actual, &target);
        // Non-empty stays as actual (array_of Int) rather than refining
        match result {
            Type::Array(inner) => assert!(matches!(*inner, Type::Int)),
            _ => panic!("expected Array"),
        }
    }

    #[test]
    fn test_refine_literal_type_non_array_unchanged() {
        let c = Checker::new();
        let expr = ident("x");
        let result = c.refine_literal_type(&expr, &Type::Int, &Type::Float);
        assert!(matches!(result, Type::Int));
    }
}
