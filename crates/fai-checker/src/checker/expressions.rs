//! Expression-level type checking: lookups, calls, member access, binary ops, generic binding.

use std::collections::HashMap;

use fai_compiler::ast::*;

use super::resolve::{apply_generic_bindings, is_numeric};
use super::Checker;
use crate::environment::Environment;
use crate::error::CheckError;
use crate::types::*;

/// One-line rendering of a function signature, used inline in
/// "missing required argument" / "unknown parameter" errors so an
/// agent doesn't have to bounce out to `fai doc` to see what the
/// function expects.
///
/// Shape: `name(p1: T1, p2: T2, ...) -> R`. Parameters with default
/// values are marked `?` after the type. The return type is shown
/// when it isn't `Void`.
fn format_function_signature(sig: &FunctionSig) -> String {
    let params: Vec<String> = sig
        .params
        .iter()
        .map(|p| {
            let opt = if p.has_default { "?" } else { "" };
            format!("{}: {}{}", p.name, describe_type(&p.ty), opt)
        })
        .collect();
    let returns_void = matches!(sig.returns.first(), Some(Type::Void));
    if returns_void || sig.returns.is_empty() {
        format!("{}({})", sig.name, params.join(", "))
    } else {
        let ret = describe_type(&sig.returns[0]);
        format!("{}({}) -> {}", sig.name, params.join(", "), ret)
    }
}

/// Hint suffix appended to argument-type-mismatch errors when the
/// shape matches the canonical agent footgun:
/// `useSignal(initial) do loader end` where `initial` was `null`
/// (or empty) so `useSignal` inferred `T = null` and the loader's
/// real return type doesn't match. Agents typically try to "fix the
/// loader" — the actual fix is a typed initial value.
///
/// Returns empty string when the call doesn't match the pattern, so
/// the error is unchanged for unrelated mismatches.
#[allow(non_snake_case)]
fn useSignal_loader_hint(
    fn_name: &str,
    param_name: &str,
    expected: &Type,
    actual: &Type,
) -> String {
    if fn_name != "useSignal" || param_name != "loader" {
        return String::new();
    }
    // Expected is `() -> X?` synthesized from initial. If X is null
    // or empty, the agent passed an untyped default. Show the fix.
    let expected_str = describe_type(expected);
    let actual_str = describe_type(actual);
    let inferred_from_null = expected_str.contains("null") || expected_str.contains("Unknown");
    if !inferred_from_null {
        return String::new();
    }
    format!(
        ".\n\n`useSignal(initial)` infers the signal's element type from `initial`. \
         You passed an untyped or null initial value, so it was inferred as `{}`. \
         The loader returns `{}`, which doesn't match.\n\n\
         Pass a typed default that matches the loader's return type:\n\n  \
         let defaultPost = Post(id: 0, title: '', body: '')\n  \
         var post = useSignal(defaultPost) do\n      \
         getPost(slug)\n  \
         end",
        expected_str, actual_str
    )
}

/// Hint shown after the generic "Cannot access property 'x' on T" error,
/// tailored to the actual type so agents see the right API to use instead
/// of guessing (or retrying the same bad pattern). Returned string starts
/// with ". " so it appends cleanly to the main error message.
fn property_access_hint(obj_type: &Type, property: &str) -> String {
    match obj_type {
        Type::Array(_) | Type::Tuple(_) => format!(
            ". Arrays have no named properties — use .items to iterate, .length for count; \
             for Dictionary lookup use getString(d, '{}') / getInt(d, '{}')",
            property, property,
        ),
        Type::String => format!(
            ". String has no properties — use string.{}(s) style functions (see `fai doc std.string`)",
            property,
        ),
        Type::Int | Type::Float | Type::Bool => ". Primitives have no properties".to_string(),
        Type::Function(_) => ". This is a function value — call it with () to invoke".to_string(),
        _ => format!(
            ". If this value is dictionary-like, use getString(d, '{}') / getInt(d, '{}') \
             / getBool(d, '{}'), or iterate d.items for arrays",
            property, property, property,
        ),
    }
}

impl Checker {
    pub(super) fn check_expression(
        &mut self,
        expr: &Expression,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        let ty = match expr {
            Expression::IdentifierExpression(ie) => Ok(env.get(&ie.name)?.ty.clone()),
            Expression::StringExpression(_) | Expression::TemplateStringExpression(_) => {
                // For template strings, we should check the embedded expressions
                if let Expression::TemplateStringExpression(ts) = expr {
                    for part in &ts.parts {
                        if let TemplateStringPart::Expression { expression } = part {
                            self.check_expression(expression, env)?;
                        }
                    }
                }
                Ok(Type::String)
            }
            Expression::BooleanExpression(_) => Ok(Type::Bool),
            Expression::NullExpression(_) => Ok(Type::Null),
            Expression::NumberExpression(ne) => {
                if ne.is_float {
                    Ok(Type::Float)
                } else if ne.value == (ne.value as i64) as f64 {
                    Ok(Type::Int)
                } else {
                    Ok(Type::Float)
                }
            }
            Expression::RangeExpression(re) => {
                let start = self.check_expression(&re.start, env)?;
                let end = self.check_expression(&re.end, env)?;
                if !same_type(&start, &Type::Int) || !same_type(&end, &Type::Int) {
                    return Err(CheckError::new(format!(
                        "Range expression requires Int bounds, got {} and {}",
                        describe_type(&start),
                        describe_type(&end)
                    )));
                }
                Ok(array_of(Type::Int))
            }
            Expression::ArrayExpression(ae) => {
                if ae.items.is_empty() {
                    return Ok(array_of(Type::Unknown));
                }
                // Walk items, unifying types as we go. `unify_branch_type`
                // already handles same-type, T + null → T?, and similar
                // narrow widening. When two items have no clean
                // unification (e.g. Int + String), the literal becomes
                // `Unknown[]` — forai is dynamically typed at runtime, and
                // mixed-type arrays (sqlite param lists, RPC arg arrays)
                // are a real callsite shape that the runtime is happy with.
                // See forai#1.
                let mut element_type = self.check_expression(&ae.items[0], env)?;
                for item in &ae.items[1..] {
                    let item_type = self.check_expression(item, env)?;
                    element_type =
                        unify_branch_type(&element_type, &item_type).unwrap_or(Type::Unknown);
                }
                Ok(array_of(element_type))
            }
            Expression::DictionaryExpression(de) => {
                // Check all entry values
                for entry in &de.entries {
                    self.check_expression(&entry.value, env)?;
                }
                Ok(Type::Dictionary)
            }
            Expression::TupleExpression(te) => {
                let items: Vec<Type> = te
                    .items
                    .iter()
                    .map(|item| self.check_expression(item, env))
                    .collect::<Result<_, _>>()?;
                Ok(tuple_of(items))
            }
            Expression::OptionalCheckExpression(oe) => {
                let inner = self.check_expression(&oe.expression, env)?;
                if !matches!(&inner, Type::Optional(_)) {
                    return Err(CheckError::new(format!(
                        "Optional check requires an optional value, got {}",
                        describe_type(&inner)
                    )));
                }
                Ok(Type::Bool)
            }
            Expression::ForceUnwrapExpression(fue) => {
                let inner = self.check_expression(&fue.expression, env)?;
                match inner {
                    Type::Optional(inner_type) => Ok(*inner_type),
                    _ => Err(CheckError::new(format!(
                        "Force unwrap requires an optional value, got {}",
                        describe_type(&inner)
                    ))),
                }
            }
            Expression::MemberExpression(me) => {
                let obj_type = self.check_expression(&me.object, env)?;
                self.check_member_access(&obj_type, &me.property)
            }
            Expression::UnaryExpression(ue) => {
                let inner = self.check_expression(&ue.expression, env)?;
                match ue.operator.as_str() {
                    "!" | "not" => {
                        if !same_type(&inner, &Type::Bool) {
                            return Err(CheckError::new(format!(
                                "Unary operator '{}' requires a Bool value, got {}",
                                ue.operator,
                                describe_type(&inner)
                            )));
                        }
                        Ok(Type::Bool)
                    }
                    "-" => {
                        if !is_numeric(&inner) {
                            return Err(CheckError::new(format!(
                                "Unary operator '-' requires a numeric value, got {}",
                                describe_type(&inner)
                            )));
                        }
                        Ok(inner)
                    }
                    op => Err(CheckError::new(format!(
                        "Unsupported unary operator '{}'",
                        op
                    ))),
                }
            }
            Expression::CallExpression(ce) => self.check_call_expression(ce, env),
            Expression::BinaryExpression(be) => self.check_binary_expression(be, env),
            Expression::IndexExpression(ie) => {
                let obj_type = self.check_expression(&ie.object, env)?;
                let idx_type = self.check_expression(&ie.index, env)?;
                match &obj_type {
                    Type::Array(inner) => {
                        if !same_type(&idx_type, &Type::Int) {
                            return Err(CheckError::new(format!(
                                "Array index must be Int, got {}",
                                describe_type(&idx_type)
                            )));
                        }
                        Ok((**inner).clone())
                    }
                    Type::Dictionary => Ok(optional_of(Type::Unknown)),
                    Type::Unknown => Ok(Type::Unknown),
                    _ => Err(CheckError::new(format!(
                        "Cannot index into {}",
                        describe_type(&obj_type)
                    ))),
                }
            }
            Expression::FunctionExpression(fd) => {
                // Check the body first to get body type
                env.push_scope();
                for param in &fd.params {
                    let param_type = self.resolve_type_node(&param.type_node)?;
                    env.define(&param.name, param_type, false)?;
                }
                let body_type = self.check_block(&fd.body, env)?;
                env.pop_scope();

                // Build params
                let params: Vec<FunctionParam> = fd
                    .params
                    .iter()
                    .map(|p| {
                        let ty = self
                            .resolve_type_node(&p.type_node)
                            .unwrap_or(Type::Unknown);
                        FunctionParam {
                            name: p.name.clone(),
                            ty,
                            has_default: p.default_value.is_some(),
                            is_mutable: p.is_mutable,
                        }
                    })
                    .collect();

                // If return types are declared, use them and validate.
                // If empty (do blocks), infer from body.
                let returns = if fd.return_types.is_empty() {
                    vec![body_type]
                } else {
                    let declared: Vec<Type> = fd
                        .return_types
                        .iter()
                        .map(|rd| self.resolve_type_node(&rd.type_node))
                        .collect::<Result<_, _>>()?;
                    // Validate body matches declared return
                    if declared.len() == 1 {
                        if !is_assignable(&body_type, &declared[0]) {
                            return Err(CheckError::new(format!(
                                "Function '{}' returns {} but expected {}",
                                fd.name,
                                describe_type(&body_type),
                                describe_type(&declared[0])
                            )));
                        }
                    }
                    declared
                };

                Ok(function_type(&fd.name, params, returns))
            }
        }?;

        let module_key = self.location_key();
        let key = crate::checker::expression_key(expr, module_key);
        self.expression_types.insert(key, ty.clone());
        Ok(ty)
    }

    fn check_member_access(&self, obj_type: &Type, property: &str) -> Result<Type, CheckError> {
        match obj_type {
            Type::EnumNamespace(name) => {
                let members = self
                    .enum_declarations
                    .get(name)
                    .ok_or_else(|| CheckError::new(format!("Unknown enum '{}'", name)))?;
                if !members.contains(&property.to_string()) {
                    return Err(CheckError::new(format!(
                        "Unknown enum member '{}' on '{}'",
                        property, name
                    )));
                }
                Ok(named_type(name, NamedCategory::Enum))
            }
            Type::ModuleNamespace { name, exports } => {
                exports.get(property).cloned().ok_or_else(|| {
                    CheckError::new(format!(
                        "Unknown export '{}' on module '{}'",
                        property, name
                    ))
                })
            }
            Type::Named {
                name,
                category: NamedCategory::Type,
                generic_bindings,
            } => {
                let fields = self
                    .type_fields
                    .get(name)
                    .ok_or_else(|| CheckError::new(format!("Unknown type '{}'", name)))?;
                let field_type = fields.get(property).cloned().ok_or_else(|| {
                    CheckError::new(format!("Unknown field '{}' on type '{}'", property, name))
                })?;
                // Apply generic bindings to resolve TypeParameter fields
                Ok(apply_generic_bindings(&field_type, generic_bindings))
            }
            Type::Error => {
                let fields = self.type_fields.get("Error").ok_or_else(|| {
                    CheckError::new("Error type fields not initialized".to_string())
                })?;
                fields.get(property).cloned().ok_or_else(|| {
                    CheckError::new(format!("Unknown field '{}' on type 'Error'", property))
                })
            }
            // Generic type parameters and Unknown: allow field access, resolve at runtime
            Type::TypeParameter(_) | Type::Unknown => Ok(Type::Unknown),
            // Dictionary: allow arbitrary property access (dynamic type, returns Unknown)
            Type::Dictionary => Ok(Type::Unknown),
            _ => Err(CheckError::new(format!(
                "Cannot access property '{}' on {}{}",
                property,
                describe_type(obj_type),
                property_access_hint(obj_type, property),
            ))),
        }
    }

    /// Check a call expression, passing an optional expected-return-type hint
    /// so that unresolved `@type T` params can be inferred from an LHS annotation.
    pub(super) fn check_call_expression_with_hint(
        &mut self,
        ce: &CallExpression,
        env: &mut Environment,
        hint: Option<Type>,
    ) -> Result<Type, CheckError> {
        self.check_call_expression_inner(ce, env, hint)
    }

    fn check_call_expression(
        &mut self,
        ce: &CallExpression,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        self.check_call_expression_inner(ce, env, None)
    }

    fn check_call_expression_inner(
        &mut self,
        ce: &CallExpression,
        env: &mut Environment,
        return_hint: Option<Type>,
    ) -> Result<Type, CheckError> {
        // UFCS: if callee is x.foo(args) and foo is not a field on x's type,
        // try rewriting to foo(x, args) — a free function call.
        if let Expression::MemberExpression(me) = &*ce.callee {
            let obj_type = self.check_expression(&me.object, env)?;
            let field_result = self.check_member_access(&obj_type, &me.property);

            if field_result.is_err() {
                // Field doesn't exist — try UFCS lookup
                if let Ok(binding) = env.get(&me.property) {
                    if let Type::Function(_sig) = &binding.ty {
                        // Found a free function with this name. Rewrite:
                        // x.foo(a, b) => foo(x, a, b)
                        let module_key = self.location_key();
                        self.ufcs_calls
                            .insert((module_key, ce.location.line, ce.location.column));
                        let mut ufcs_args = Vec::with_capacity(1 + ce.args.len());
                        ufcs_args.push(CallArgument {
                            label: None,
                            value: *me.object.clone(),
                            location: me.location.clone(),
                        });
                        ufcs_args.extend(ce.args.iter().cloned());

                        let ufcs_call = CallExpression {
                            callee: Box::new(Expression::IdentifierExpression(
                                IdentifierExpression {
                                    name: me.property.clone(),
                                    location: me.location.clone(),
                                },
                            )),
                            args: ufcs_args,
                            location: ce.location.clone(),
                        };

                        return self.check_call_expression_inner(&ufcs_call, env, return_hint);
                    }
                }
                // No UFCS candidate found.
                // Give a more actionable error: if the name looks like a known
                // Forui view modifier or signal helper that simply wasn't imported,
                // say so explicitly instead of reporting a confusing field-access error.
                let fn_name = &me.property;
                let forui_view_fns = [
                    "padding",
                    "background",
                    "foreground",
                    "cornerRadius",
                    "fontSize",
                    "fontWeight",
                    "flex",
                    "opacity",
                    "withKey",
                    "onClick",
                    "onChange",
                    "scrollView",
                ];
                let forui_signal_fns = [
                    "isLoading",
                    "isLoaded",
                    "isError",
                    "isInitial",
                    "reload",
                    "setValue",
                    "setLoading",
                    "setLoaded",
                    "setError",
                ];
                let forui_router_fns = ["navigate", "currentPath", "routeParam"];
                if forui_view_fns.contains(&fn_name.as_str()) {
                    return Err(CheckError::new(format!(
                        "'{}' is not in scope — add it to your `use {{ ... }} from Forui.view` import",
                        fn_name
                    )));
                } else if forui_signal_fns.contains(&fn_name.as_str()) {
                    return Err(CheckError::new(format!(
                        "'{}' is not in scope — add it to your `use {{ ... }} from Forui.signal` import",
                        fn_name
                    )));
                } else if forui_router_fns.contains(&fn_name.as_str()) {
                    return Err(CheckError::new(format!(
                        "'{}' is not in scope — add it to your `use {{ ... }} from Forui.router` import",
                        fn_name
                    )));
                }
                // Fall back to the original field error for everything else.
                // Also hint that `fai doc <name>` can find the right module.
                let mut err = field_result.unwrap_err();
                err.message = format!(
                    "{} (if '{}' is a function from an imported module, make sure it is listed in your `use {{ ... }}` import — run `fai doc {}` to find it)",
                    err.message, fn_name, fn_name
                );
                return Err(err);
            }
            // Field exists — fall through to normal call path
        }

        let callee_type = self.check_expression(&ce.callee, env)?;

        match &callee_type {
            Type::TypeConstructor(name) => self.check_type_construction(name, ce, env),
            Type::Function(sig) => self.check_function_call(sig.clone(), ce, env, return_hint),
            _ => Err(CheckError::new(format!(
                "Cannot call value of type {}",
                describe_type(&callee_type)
            ))),
        }
    }

    fn check_type_construction(
        &mut self,
        type_name: &str,
        ce: &CallExpression,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        let decl = self
            .type_declarations
            .get(type_name)
            .cloned()
            .ok_or_else(|| CheckError::new(format!("Unknown type '{}'", type_name)))?;

        let mut args_by_label = HashMap::new();
        for arg in &ce.args {
            if let Some(label) = &arg.label {
                let arg_type = self.check_expression(&arg.value, env)?;
                args_by_label.insert(label.clone(), arg_type);
            } else {
                return Err(CheckError::new(format!(
                    "Type construction for '{}' requires labeled arguments",
                    type_name
                )));
            }
        }

        let mut generic_bindings: HashMap<String, Type> = HashMap::new();

        for field in &decl.fields {
            let field_type = self.resolve_type_node(&field.type_node)?;
            if let Some(actual) = args_by_label.get(&field.name) {
                if !self.bind_and_check_assignable(actual, &field_type, &mut generic_bindings) {
                    let expected = apply_generic_bindings(&field_type, &generic_bindings);
                    return Err(CheckError::new(format!(
                        "Field '{}' on '{}' expects {}, got {}",
                        field.name,
                        type_name,
                        describe_type(&expected),
                        describe_type(actual)
                    )));
                }
            } else if field.default_value.is_none() && !field.type_node.is_optional {
                return Err(CheckError::new(format!(
                    "Missing required field '{}' for type '{}'",
                    field.name, type_name
                )));
            }
        }

        // Reject labels that don't name a declared field — silently
        // dropping them used to be legal and led to typos going
        // undiagnosed.
        for label in args_by_label.keys() {
            if !decl.fields.iter().any(|f| &f.name == label) {
                return Err(CheckError::new(format!(
                    "Unknown field '{}' on type '{}'",
                    label, type_name
                )));
            }
        }

        Ok(named_type_with_bindings(
            type_name,
            NamedCategory::Type,
            generic_bindings,
        ))
    }

    fn check_function_call(
        &mut self,
        sig: FunctionSig,
        ce: &CallExpression,
        env: &mut Environment,
        return_hint: Option<Type>,
    ) -> Result<Type, CheckError> {
        // Special arg count checks for mock/assert builtins
        let name = &sig.name;
        if name == "mock" || name == "mockOnce" {
            if ce.args.len() < 2 {
                return Err(CheckError::new(format!(
                    "Missing required arguments for '{}'",
                    name
                )));
            }
        } else if name == "assertCalledWith" {
            if ce.args.is_empty() {
                return Err(CheckError::new(format!(
                    "Missing required arguments for '{}'",
                    name
                )));
            }
            for arg in &ce.args {
                self.check_expression(&arg.value, env)?;
            }
            return Ok(Type::Void);
        } else if name == "assertCallCount" {
            if ce.args.len() != 2 {
                return Err(CheckError::new(
                    "'assert.callCount' expects exactly 2 arguments".to_string(),
                ));
            }
        } else if name == "assertNotCalled" {
            if ce.args.len() != 1 {
                return Err(CheckError::new(
                    "'assert.notCalled' expects exactly 1 argument".to_string(),
                ));
            }
        } else if name == "mockReset" {
            if ce.args.len() != 1 {
                return Err(CheckError::new(
                    "'mockReset' expects exactly 1 argument".to_string(),
                ));
            }
        } else if name == "all" {
            // all() is variadic — accept any number of args, return Unknown tuple
            for arg in &ce.args {
                self.check_expression(&arg.value, env)?;
            }
            return Ok(Type::Unknown);
        } else if ce.args.len() > sig.params.len() && !ce.args.iter().any(|a| a.label.is_some()) {
            // Only check arg count for purely positional calls.
            // Named param calls are validated during resolution below.
            return Err(CheckError::new(format!(
                "Too many arguments for '{}'",
                name
            )));
        }

        let mut generic_bindings: HashMap<String, Type> = HashMap::new();

        // Named parameter support: resolve args to param positions.
        // Rule: positional args first, once a named arg appears, all following must be named.
        // resolved_args[param_idx] = Some(arg)
        let mut resolved_args: Vec<Option<&CallArgument>> = vec![None; sig.params.len()];
        // arg_to_param[param_idx] = Some(arg_idx) -- for compiler reordering
        let mut arg_to_param: Vec<Option<usize>> = vec![None; sig.params.len()];
        let mut seen_named = false;
        let mut positional_idx = 0;
        let mut needs_reorder = false;

        for (arg_idx, arg) in ce.args.iter().enumerate() {
            if let Some(label) = &arg.label {
                seen_named = true;
                // Find the param with this label
                let param_idx = sig.params.iter().position(|p| p.name == *label);
                match param_idx {
                    Some(idx) => {
                        if resolved_args[idx].is_some() {
                            return Err(CheckError::new(format!(
                                "Duplicate argument '{}' for '{}'",
                                label, sig.name
                            )));
                        }
                        resolved_args[idx] = Some(arg);
                        arg_to_param[idx] = Some(arg_idx);
                        if idx != arg_idx {
                            needs_reorder = true;
                        }
                    }
                    None => {
                        return Err(CheckError::new(format!(
                            "Unknown parameter '{}' for '{}'",
                            label, sig.name
                        )));
                    }
                }
            } else {
                if seen_named {
                    return Err(CheckError::new(format!(
                        "Positional argument after named argument in call to '{}'",
                        sig.name
                    )));
                }
                if positional_idx >= sig.params.len() {
                    return Err(CheckError::new(format!(
                        "Too many arguments for '{}'",
                        sig.name
                    )));
                }
                resolved_args[positional_idx] = Some(arg);
                arg_to_param[positional_idx] = Some(arg_idx);
                positional_idx += 1;
            }
        }

        // Record reordering if needed (named params out of definition order)
        if needs_reorder || (seen_named && ce.args.iter().any(|a| a.label.is_some())) {
            let module_key = self.location_key();
            self.named_param_reorder.insert(
                (module_key, ce.location.line, ce.location.column),
                arg_to_param.clone(),
            );
        }

        for (i, param) in sig.params.iter().enumerate() {
            let arg = resolved_args[i];
            if arg.is_none() {
                if !param.has_default {
                    // Inline the function's full signature so the
                    // agent can fix in one turn — without the
                    // signature they have to round-trip via
                    // `fai doc <name>` to see what's expected.
                    let signature = format_function_signature(&sig);
                    return Err(CheckError::new(format!(
                        "Missing required argument '{}' for '{}'.\n\n  \
                         {}\n\n\
                         Run `fai doc {}` for the full signature including defaults.",
                        param.name, sig.name, signature, sig.name
                    )));
                }
                continue;
            }
            let arg_expr = &arg.unwrap().value;
            let actual = self.check_expression(arg_expr, env)?;
            if !self.accept_builtin_special_case(&sig.name, i, &actual, ce, env) {
                if !self.bind_and_check_assignable(&actual, &param.ty, &mut generic_bindings) {
                    let expected = apply_generic_bindings(&param.ty, &generic_bindings);
                    // Targeted hint for the most common Forui shape
                    // agents trip on: `useSignal(initial) do loader end`.
                    // When the loader's return type doesn't match the
                    // signal's element type, the root cause is almost
                    // always that `initial` was `null` or an untyped
                    // empty value, so the signal was inferred as
                    // `null?` / `empty[]`. Tell the agent to pass a
                    // typed default rather than "fix the loader."
                    let hint = useSignal_loader_hint(&sig.name, &param.name, &expected, &actual);
                    return Err(CheckError::new(format!(
                        "Argument '{}' for '{}' expects {}, got {}{}",
                        param.name,
                        sig.name,
                        describe_type(&expected),
                        describe_type(&actual),
                        hint,
                    )));
                }
            }
            // Mutable param check: caller must pass a var binding
            if param.is_mutable {
                if let Expression::IdentifierExpression(id) = arg_expr {
                    if let Ok(binding) = env.get(&id.name) {
                        if !binding.mutable {
                            return Err(CheckError::new(format!(
                                "Cannot pass immutable '{}' to mutable parameter '{}' of '{}'. Use 'var' instead of 'let'",
                                id.name, param.name, sig.name
                            )));
                        }
                    }
                }
            }
        }

        // Special return type handling
        // arrayReverse / arraySlice / arraySort all return T[] where T is the
        // element type of arg 0. Their argument check goes through
        // accept_builtin_special_case which doesn't populate generic_bindings,
        // so the return type would otherwise come out as $T[]. Re-derive from
        // the first argument the same way append does.
        if sig.name == "append"
            || sig.name == "arraySort"
            || sig.name == "arraySlice"
            || sig.name == "arrayReverse"
        {
            return self.check_expression(&ce.args[0].value, env);
        }
        if sig.name == "unwrap" {
            let opt_type = self.check_expression(&ce.args[0].value, env)?;
            let fallback_type = self.check_expression(&ce.args[1].value, env)?;
            match &opt_type {
                Type::Optional(inner) => {
                    if !is_assignable(&fallback_type, inner) {
                        return Err(CheckError::new(format!(
                            "unwrap(value, fallback) fallback expects {}, got {}",
                            describe_type(inner),
                            describe_type(&fallback_type)
                        )));
                    }
                    return Ok((**inner).clone());
                }
                _ => {
                    return Err(CheckError::new(format!(
                        "unwrap(value, fallback) requires an optional as the first argument, got {}",
                        describe_type(&opt_type)
                    )));
                }
            }
        }

        // Infer remaining unresolved @type params from the LHS annotation hint.
        // e.g., if the function returns T[] and the hint is Task[], bind T=Task.
        if let Some(ref hint) = return_hint {
            if let Some(first_return) = sig.returns.first() {
                for tp_name in &sig.type_params {
                    if !generic_bindings.contains_key(tp_name) {
                        // bind_and_check_assignable(actual=hint, expected=T[]) binds T=Task
                        let mut temp = generic_bindings.clone();
                        self.bind_and_check_assignable(hint, first_return, &mut temp);
                        for (k, v) in temp {
                            generic_bindings.entry(k).or_insert(v);
                        }
                        break; // re-check after each inference round
                    }
                }
            }
        }

        // Record resolved type constructor names for the compiler to inject.
        if !sig.type_params.is_empty() {
            let resolved_names: Vec<String> = sig
                .type_params
                .iter()
                .map(|tp_name| match generic_bindings.get(tp_name) {
                    Some(Type::Named { name: n, .. }) => n.clone(),
                    Some(Type::TypeConstructor(n)) => n.clone(),
                    _ => String::new(),
                })
                .collect();
            if resolved_names.iter().any(|s| !s.is_empty()) {
                let module_key = self.location_key();
                self.generic_type_args.insert(
                    (module_key, ce.location.line, ce.location.column),
                    resolved_names,
                );
            }
        }

        let resolved: Vec<Type> = sig
            .returns
            .iter()
            .map(|r| apply_generic_bindings(r, &generic_bindings))
            .collect();

        if resolved.len() > 1 {
            return Ok(tuple_of(resolved));
        }
        Ok(resolved.into_iter().next().unwrap_or(Type::Unknown))
    }

    fn accept_builtin_special_case(
        &mut self,
        name: &str,
        index: usize,
        actual: &Type,
        ce: &CallExpression,
        env: &mut Environment,
    ) -> bool {
        match name {
            "length" | "isEmpty" => {
                same_type(actual, &Type::String)
                    || same_type(actual, &Type::Dictionary)
                    || matches!(actual, Type::Array(_))
            }
            "replace" | "split" | "trim" | "toUpper" | "toLower" | "stringContains"
            | "stringStartsWith" | "stringEndsWith" | "stringSubstring" | "stringIndexOf"
            | "stringRepeat" | "stringTrimStart" | "stringTrimEnd" | "htmlEscape" => {
                same_type(actual, &Type::String) || same_type(actual, &Type::Int)
            }
            "jsonRequireString" => {
                if index == 0 {
                    same_type(actual, &Type::Dictionary)
                } else if index == 1 {
                    same_type(actual, &Type::String)
                } else {
                    false
                }
            }
            "arraySort" | "arrayReverse" | "arraySlice" => {
                matches!(actual, Type::Array(_)) || same_type(actual, &Type::Int)
            }
            "arrayIndexOf" | "arrayJoin" | "stringJoin" => {
                matches!(actual, Type::Array(_))
                    || same_type(actual, &Type::String)
                    || same_type(actual, &Type::Unknown)
            }
            "toInt" | "toFloat" | "toBool" => {
                same_type(actual, &Type::Int) || same_type(actual, &Type::Float)
            }
            "mathFloor" | "mathCeil" | "mathRound" | "mathAbs" | "mathSqrt" | "mathMin"
            | "mathMax" | "mathPow" => {
                same_type(actual, &Type::Int) || same_type(actual, &Type::Float)
            }
            "append" => {
                if index == 0 {
                    matches!(actual, Type::Array(_))
                } else if index == 1 {
                    if let Ok(array_type) = self.check_expression(&ce.args[0].value, env) {
                        if let Type::Array(inner) = &array_type {
                            return is_assignable(actual, inner);
                        }
                    }
                    false
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn check_binary_expression(
        &mut self,
        be: &BinaryExpression,
        env: &mut Environment,
    ) -> Result<Type, CheckError> {
        let left = self.check_expression(&be.left, env)?;
        let right = self.check_expression(&be.right, env)?;

        match be.operator.as_str() {
            "and" | "or" => {
                if !same_type(&left, &Type::Bool) || !same_type(&right, &Type::Bool) {
                    return Err(CheckError::new(format!(
                        "Operator '{}' requires Bool operands",
                        be.operator
                    )));
                }
                Ok(Type::Bool)
            }
            "==" | "!=" => {
                let compares_nullable = (matches!(left, Type::Optional(_) | Type::Ptr(_))
                    && matches!(right, Type::Null))
                    || (matches!(right, Type::Optional(_) | Type::Ptr(_))
                        && matches!(left, Type::Null));
                let has_unknown = matches!(left, Type::Unknown | Type::TypeParameter(_))
                    || matches!(right, Type::Unknown | Type::TypeParameter(_));
                if !same_type(&left, &right) && !compares_nullable && !has_unknown {
                    return Err(CheckError::new(format!(
                        "Cannot compare {} and {}",
                        describe_type(&left),
                        describe_type(&right)
                    )));
                }
                Ok(Type::Bool)
            }
            ">" | ">=" | "<" | "<=" => {
                // Defer to runtime when either side's type is erased
                // (Unknown or unbound TypeParameter from a generic field).
                let has_unknown = matches!(left, Type::Unknown | Type::TypeParameter(_))
                    || matches!(right, Type::Unknown | Type::TypeParameter(_));
                if has_unknown {
                    return Ok(Type::Bool);
                }
                let both_strings =
                    same_type(&left, &Type::String) && same_type(&right, &Type::String);
                if !both_strings && (!is_numeric(&left) || !is_numeric(&right)) {
                    return Err(CheckError::new(format!(
                        "Comparison operator '{}' requires numeric or string types",
                        be.operator
                    )));
                }
                Ok(Type::Bool)
            }
            "+" => {
                // Defer to runtime when either side's type is erased.
                // Without this, e.g. `'name: ' + sig.value` fails when
                // `sig` is a `@param sig Signal` whose `value $T` field
                // can't be locally re-resolved to String.
                let has_unknown = matches!(left, Type::Unknown | Type::TypeParameter(_))
                    || matches!(right, Type::Unknown | Type::TypeParameter(_));
                if has_unknown {
                    // Best-effort: if the OTHER side is a known concrete
                    // type, return that so chains like `'a' + b.value +
                    // 'c'` keep flowing as String.
                    return Ok(match (&left, &right) {
                        (Type::String, _) | (_, Type::String) => Type::String,
                        (Type::Int, _) | (_, Type::Int) => Type::Int,
                        (Type::Float, _) | (_, Type::Float) => Type::Float,
                        _ => Type::Unknown,
                    });
                }
                // String concatenation
                if same_type(&left, &Type::String) && same_type(&right, &Type::String) {
                    return Ok(Type::String);
                }
                if !is_numeric(&left) || !is_numeric(&right) {
                    return Err(CheckError::new(format!(
                        "Operator '+' requires numeric or string operands, got {} and {}",
                        describe_type(&left),
                        describe_type(&right)
                    )));
                }
                if same_type(&left, &Type::Int) && same_type(&right, &Type::Int) {
                    Ok(Type::Int)
                } else {
                    Ok(Type::Float)
                }
            }
            "-" | "*" | "%" => {
                let has_unknown = matches!(left, Type::Unknown | Type::TypeParameter(_))
                    || matches!(right, Type::Unknown | Type::TypeParameter(_));
                if has_unknown {
                    return Ok(match (&left, &right) {
                        (Type::Int, _) | (_, Type::Int) => Type::Int,
                        (Type::Float, _) | (_, Type::Float) => Type::Float,
                        _ => Type::Unknown,
                    });
                }
                if !is_numeric(&left) || !is_numeric(&right) {
                    return Err(CheckError::new(format!(
                        "Operator '{}' requires numeric operands, got {} and {}",
                        be.operator,
                        describe_type(&left),
                        describe_type(&right)
                    )));
                }
                if same_type(&left, &Type::Int) && same_type(&right, &Type::Int) {
                    Ok(Type::Int)
                } else {
                    Ok(Type::Float)
                }
            }
            "/" => {
                let has_unknown = matches!(left, Type::Unknown | Type::TypeParameter(_))
                    || matches!(right, Type::Unknown | Type::TypeParameter(_));
                if has_unknown {
                    return Ok(Type::Float);
                }
                if !is_numeric(&left) || !is_numeric(&right) {
                    return Err(CheckError::new(format!(
                        "Operator '/' requires numeric operands, got {} and {}",
                        describe_type(&left),
                        describe_type(&right)
                    )));
                }
                Ok(Type::Float)
            }
            "//" | "**" => {
                let has_unknown = matches!(left, Type::Unknown | Type::TypeParameter(_))
                    || matches!(right, Type::Unknown | Type::TypeParameter(_));
                if has_unknown {
                    return Ok(match (&left, &right) {
                        (Type::Int, _) | (_, Type::Int) => Type::Int,
                        (Type::Float, _) | (_, Type::Float) => Type::Float,
                        _ => Type::Unknown,
                    });
                }
                if !is_numeric(&left) || !is_numeric(&right) {
                    return Err(CheckError::new(format!(
                        "Operator '{}' requires numeric operands, got {} and {}",
                        be.operator,
                        describe_type(&left),
                        describe_type(&right)
                    )));
                }
                if same_type(&left, &Type::Int) && same_type(&right, &Type::Int) {
                    Ok(Type::Int)
                } else {
                    Ok(Type::Float)
                }
            }
            op => Err(CheckError::new(format!("Unsupported operator '{}'", op))),
        }
    }

    // ── Generic type binding ─────────────────────────────────────────

    fn bind_and_check_assignable(
        &self,
        actual: &Type,
        expected: &Type,
        bindings: &mut HashMap<String, Type>,
    ) -> bool {
        if let Type::TypeParameter(name) = expected {
            if let Some(existing) = bindings.get(name) {
                if same_type(actual, existing) {
                    return true;
                }
                // Refine Unknown bindings: if T was bound to Unknown
                // (from an untyped []), a later arg with a concrete type
                // should update the binding.
                if contains_unknown(existing) && !contains_unknown(actual) {
                    bindings.insert(name.clone(), actual.clone());
                    return true;
                }
                return is_assignable(actual, existing);
            }
            bindings.insert(name.clone(), actual.clone());
            return true;
        }

        if let (Type::Array(a_inner), Type::Array(e_inner)) = (actual, expected) {
            return self.bind_and_check_assignable(a_inner, e_inner, bindings);
        }

        // Ptr accepts Ptr? and null (opaque handles are nullable)
        if let Type::Ptr(_) = expected {
            if matches!(actual, Type::Null) {
                return true;
            }
            if let Type::Optional(a_inner) = actual {
                return self.bind_and_check_assignable(a_inner, expected, bindings);
            }
        }

        if let Type::Optional(e_inner) = expected {
            if matches!(actual, Type::Null) {
                return true;
            }
            if let Type::Optional(a_inner) = actual {
                return self.bind_and_check_assignable(a_inner, e_inner, bindings);
            }
            return self.bind_and_check_assignable(actual, e_inner, bindings);
        }

        if let (Type::Tuple(a_items), Type::Tuple(e_items)) = (actual, expected) {
            return a_items.len() == e_items.len()
                && a_items
                    .iter()
                    .zip(e_items.iter())
                    .all(|(a, e)| self.bind_and_check_assignable(a, e, bindings));
        }

        if let (Type::Function(a_sig), Type::Function(e_sig)) = (actual, expected) {
            let params_ok = a_sig.params.len() == e_sig.params.len()
                && a_sig
                    .params
                    .iter()
                    .zip(e_sig.params.iter())
                    .all(|(a, e)| self.bind_and_check_assignable(&a.ty, &e.ty, bindings));
            let expected_void = e_sig.returns.len() == 1 && matches!(e_sig.returns[0], Type::Void);
            let returns_ok = expected_void
                || (a_sig.returns.len() == e_sig.returns.len()
                    && a_sig
                        .returns
                        .iter()
                        .zip(e_sig.returns.iter())
                        .all(|(a, e)| self.bind_and_check_assignable(a, e, bindings)));
            return params_ok && returns_ok;
        }

        is_assignable(actual, expected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bind_and_check_assignable_type_param_first_binds() {
        let c = Checker::new();
        let mut bindings = HashMap::new();
        let ok = c.bind_and_check_assignable(&Type::Int, &type_parameter("T"), &mut bindings);
        assert!(ok);
        assert!(matches!(bindings.get("T"), Some(Type::Int)));
    }

    #[test]
    fn test_bind_and_check_assignable_type_param_second_must_match() {
        let c = Checker::new();
        let mut bindings = HashMap::new();
        bindings.insert("T".to_string(), Type::Int);
        assert!(c.bind_and_check_assignable(&Type::Int, &type_parameter("T"), &mut bindings));
        assert!(!c.bind_and_check_assignable(&Type::String, &type_parameter("T"), &mut bindings));
    }

    #[test]
    fn test_bind_and_check_assignable_array_recurses() {
        let c = Checker::new();
        let mut bindings = HashMap::new();
        let ok = c.bind_and_check_assignable(
            &array_of(Type::Int),
            &array_of(type_parameter("T")),
            &mut bindings,
        );
        assert!(ok);
        assert!(matches!(bindings.get("T"), Some(Type::Int)));
    }

    #[test]
    fn test_bind_and_check_assignable_optional_accepts_null() {
        let c = Checker::new();
        let mut bindings = HashMap::new();
        let ok = c.bind_and_check_assignable(&Type::Null, &optional_of(Type::Int), &mut bindings);
        assert!(ok);
    }

    #[test]
    fn test_bind_and_check_assignable_ptr_accepts_null() {
        let c = Checker::new();
        let mut bindings = HashMap::new();
        let ok = c.bind_and_check_assignable(
            &Type::Null,
            &Type::Ptr("Handle".to_string()),
            &mut bindings,
        );
        assert!(ok);
    }

    #[test]
    fn test_bind_and_check_assignable_concrete() {
        let c = Checker::new();
        let mut bindings = HashMap::new();
        assert!(c.bind_and_check_assignable(&Type::Int, &Type::Int, &mut bindings));
        assert!(!c.bind_and_check_assignable(&Type::Int, &Type::String, &mut bindings));
    }

    #[test]
    fn test_bind_and_check_assignable_tuple_matches() {
        let c = Checker::new();
        let mut bindings = HashMap::new();
        let ok = c.bind_and_check_assignable(
            &tuple_of(vec![Type::Int, Type::String]),
            &tuple_of(vec![type_parameter("T"), Type::String]),
            &mut bindings,
        );
        assert!(ok);
        assert!(matches!(bindings.get("T"), Some(Type::Int)));
    }

    #[test]
    fn test_bind_and_check_assignable_tuple_length_mismatch() {
        let c = Checker::new();
        let mut bindings = HashMap::new();
        let ok = c.bind_and_check_assignable(
            &tuple_of(vec![Type::Int, Type::String]),
            &tuple_of(vec![Type::Int]),
            &mut bindings,
        );
        assert!(!ok);
    }

    #[test]
    fn test_bind_and_check_assignable_function_void_return_allows_any() {
        let c = Checker::new();
        let mut bindings = HashMap::new();
        let actual = function_type("f", vec![], vec![Type::Int]);
        let expected = function_type("g", vec![], vec![Type::Void]);
        let ok = c.bind_and_check_assignable(&actual, &expected, &mut bindings);
        assert!(ok);
    }

    #[test]
    fn test_check_member_access_error_message_field() {
        let mut c = Checker::new();
        c.resolve_type_fields().unwrap();
        let result = c.check_member_access(&Type::Error, "message").unwrap();
        assert!(matches!(result, Type::String));
    }

    #[test]
    fn test_check_member_access_error_unknown_field() {
        let mut c = Checker::new();
        c.resolve_type_fields().unwrap();
        let result = c.check_member_access(&Type::Error, "bogus");
        assert!(result.is_err());
    }

    #[test]
    fn test_check_member_access_unknown_type_returns_unknown() {
        let c = Checker::new();
        let result = c.check_member_access(&Type::Unknown, "anything").unwrap();
        assert!(matches!(result, Type::Unknown));
    }

    #[test]
    fn test_check_member_access_non_compound_fails() {
        let c = Checker::new();
        let result = c.check_member_access(&Type::Int, "x");
        assert!(result.is_err());
    }

    #[test]
    fn test_check_member_access_module_namespace() {
        let c = Checker::new();
        let mut exports = HashMap::new();
        exports.insert("foo".to_string(), Type::Int);
        let ns = Type::ModuleNamespace {
            name: "m".to_string(),
            exports,
        };
        let result = c.check_member_access(&ns, "foo").unwrap();
        assert!(matches!(result, Type::Int));
        let result = c.check_member_access(&ns, "missing");
        assert!(result.is_err());
    }
}
