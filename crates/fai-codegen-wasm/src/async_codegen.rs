//! Async-effectful WASM backend entry point.
//!
//! This module is the public home for real async lowering. The current
//! implementation still delegates supported shapes to the legacy minimal
//! emitter while the full frame/scheduler compiler is built out behind this
//! facade. Keeping the legacy path here, rather than in `lib.rs`, makes the
//! remaining replacement work explicit and keeps production routing stable.

use std::collections::HashMap;

use fai_compiler::{
    ast::{
        AssignmentStatement, AssignmentTarget, BinaryExpression, CallExpression, Expression,
        ForStatement, FunctionDeclaration, IfStatement, MemberExpression, NowaitStatement, Program,
        ReturnStatement, Statement, ThrowStatement, TryStatement, VarStatement, WhileStatement,
    },
    compiler::DiscoveredModule,
};

use crate::{async_analysis::AsyncAnalysis, direct, LocatedBuildError};

#[derive(Debug)]
pub enum AsyncBuildOutcome {
    Compiled(Vec<u8>),
    Unsupported(LocatedBuildError),
}

pub fn try_codegen_async(
    ast: &fai_compiler::ast::Program,
    modules: &[DiscoveredModule],
    analysis: &AsyncAnalysis,
    is_test: bool,
) -> Option<AsyncBuildOutcome> {
    if analysis.is_empty() || is_test {
        return None;
    }

    if let Some(wasm) = crate::async_wait_codegen::try_codegen_minimal_wait_main(ast) {
        return Some(AsyncBuildOutcome::Compiled(wasm));
    }

    if !modules.is_empty() {
        let normalized = normalize_modules_for_legacy_async(ast, modules);
        if let Some(wasm) = crate::async_wait_codegen::try_codegen_minimal_wait_main(&normalized) {
            return Some(AsyncBuildOutcome::Compiled(wasm));
        }
    }

    analysis.first_cause().map(|(function, cause)| {
        AsyncBuildOutcome::Unsupported(LocatedBuildError {
            err: direct::BuildError::AsyncLoweringUnsupported {
                function: function.to_string(),
                cause: format!("{:?}", cause.kind),
            },
            file: cause.file.clone(),
            line: Some(cause.location.line),
            col: Some(cause.location.column),
            module: cause.module.clone(),
        })
    })
}

fn normalize_modules_for_legacy_async(ast: &Program, modules: &[DiscoveredModule]) -> Program {
    let module_function_exports = module_function_exports(modules);
    let entry_rewrites = call_rewrites(None, &ast.statements, modules, &module_function_exports);

    let mut statements = Vec::new();
    for stmt in &ast.statements {
        match stmt {
            Statement::FunctionDeclaration(fd) => {
                let mut fd = fd.clone();
                rewrite_function_body(&mut fd, &entry_rewrites);
                statements.push(Statement::FunctionDeclaration(fd));
            }
            Statement::UseStatement(_)
            | Statement::TestDeclaration(_)
            | Statement::FunctionTypeDefDeclaration(_)
            | Statement::TypeDeclaration(_)
            | Statement::EnumDeclaration(_)
            | Statement::ExternBlockDeclaration(_)
            | Statement::LetStatement(_)
            | Statement::VarStatement(_)
            | Statement::AssignmentStatement(_)
            | Statement::IfStatement(_)
            | Statement::CaseStatement(_)
            | Statement::TryStatement(_)
            | Statement::ThrowStatement(_)
            | Statement::ForStatement(_)
            | Statement::WhileStatement(_)
            | Statement::BreakStatement(_)
            | Statement::ContinueStatement(_)
            | Statement::ReturnStatement(_)
            | Statement::ExpressionStatement(_)
            | Statement::NowaitStatement(_) => {}
        }
    }

    for module in modules {
        let rewrites = call_rewrites(
            Some(module.name.as_str()),
            &module.statements,
            modules,
            &module_function_exports,
        );
        for stmt in &module.statements {
            if let Statement::FunctionDeclaration(fd) = stmt {
                let mut fd = fd.clone();
                fd.name = format!("{}.{}", module.name, fd.name);
                rewrite_function_body(&mut fd, &rewrites);
                statements.push(Statement::FunctionDeclaration(fd));
            }
        }
    }

    Program {
        kind: ast.kind.clone(),
        statements,
    }
}

fn module_function_exports(modules: &[DiscoveredModule]) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    for module in modules {
        let mut names = Vec::new();
        for stmt in &module.statements {
            if let Statement::FunctionDeclaration(fd) = stmt {
                if !module.private_names.iter().any(|name| name == &fd.name) {
                    names.push(fd.name.clone());
                }
            }
        }
        out.insert(module.name.clone(), names);
    }
    out
}

fn call_rewrites(
    current_module: Option<&str>,
    statements: &[Statement],
    modules: &[DiscoveredModule],
    module_function_exports: &HashMap<String, Vec<String>>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for module in modules {
        if let Some(last) = module.name.rsplit('.').next() {
            out.entry(last.to_string()).or_insert(module.name.clone());
        }
    }
    for stmt in statements {
        if let Statement::UseStatement(use_stmt) = stmt {
            let canonical = qualify_module_path(current_module, &use_stmt.module_path);
            if use_stmt.import_all {
                if let Some(names) = module_function_exports.get(&canonical) {
                    for name in names {
                        out.insert(name.clone(), format!("{}.{}", canonical, name));
                    }
                }
                if let Some(last) = canonical.rsplit('.').next() {
                    out.insert(last.to_string(), canonical.clone());
                }
            } else if let Some(names) = &use_stmt.imported_names {
                for name in names {
                    out.insert(name.clone(), format!("{}.{}", canonical, name));
                }
                if let Some(last) = canonical.rsplit('.').next() {
                    out.insert(last.to_string(), canonical.clone());
                }
            } else if let Some(last) = canonical.rsplit('.').next() {
                out.insert(last.to_string(), canonical.clone());
            }
        }
    }
    out
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

fn rewrite_function_body(fd: &mut FunctionDeclaration, rewrites: &HashMap<String, String>) {
    for stmt in &mut fd.body {
        rewrite_statement(stmt, rewrites);
    }
}

fn rewrite_statement(stmt: &mut Statement, rewrites: &HashMap<String, String>) {
    match stmt {
        Statement::LetStatement(ls) => rewrite_expression(&mut ls.value, rewrites),
        Statement::VarStatement(VarStatement { value, .. }) => rewrite_expression(value, rewrites),
        Statement::AssignmentStatement(AssignmentStatement { target, value, .. }) => {
            match target {
                AssignmentTarget::Field { object } | AssignmentTarget::Index { object } => {
                    rewrite_expression(object, rewrites);
                }
                AssignmentTarget::Variables { .. } => {}
            }
            rewrite_expression(value, rewrites);
        }
        Statement::IfStatement(IfStatement {
            branches,
            else_branch,
            ..
        }) => {
            for branch in branches {
                rewrite_expression(&mut branch.condition, rewrites);
                for stmt in &mut branch.body {
                    rewrite_statement(stmt, rewrites);
                }
            }
            if let Some(else_branch) = else_branch {
                for stmt in else_branch {
                    rewrite_statement(stmt, rewrites);
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
                rewrite_statement(stmt, rewrites);
            }
            for stmt in catch_body {
                rewrite_statement(stmt, rewrites);
            }
            if let Some(finally_body) = finally_body {
                for stmt in finally_body {
                    rewrite_statement(stmt, rewrites);
                }
            }
        }
        Statement::ThrowStatement(ThrowStatement { expression, .. }) => {
            rewrite_expression(expression, rewrites)
        }
        Statement::ForStatement(ForStatement { items, body, .. }) => {
            rewrite_expression(items, rewrites);
            for stmt in body {
                rewrite_statement(stmt, rewrites);
            }
        }
        Statement::WhileStatement(WhileStatement {
            condition, body, ..
        }) => {
            rewrite_expression(condition, rewrites);
            for stmt in body {
                rewrite_statement(stmt, rewrites);
            }
        }
        Statement::ReturnStatement(ReturnStatement { value, .. }) => {
            if let Some(value) = value {
                rewrite_expression(value, rewrites);
            }
        }
        Statement::ExpressionStatement(es) => rewrite_expression(&mut es.expression, rewrites),
        Statement::NowaitStatement(NowaitStatement { expression, .. }) => {
            rewrite_expression(expression, rewrites)
        }
        Statement::CaseStatement(_)
        | Statement::FunctionDeclaration(_)
        | Statement::UseStatement(_)
        | Statement::TypeDeclaration(_)
        | Statement::EnumDeclaration(_)
        | Statement::TestDeclaration(_)
        | Statement::ExternBlockDeclaration(_)
        | Statement::BreakStatement(_)
        | Statement::ContinueStatement(_)
        | Statement::FunctionTypeDefDeclaration(_) => {}
    }
}

fn rewrite_expression(expr: &mut Expression, rewrites: &HashMap<String, String>) {
    match expr {
        Expression::CallExpression(call) => rewrite_call(call, rewrites),
        Expression::MemberExpression(me) => rewrite_expression(&mut me.object, rewrites),
        Expression::BinaryExpression(BinaryExpression { left, right, .. }) => {
            rewrite_expression(left, rewrites);
            rewrite_expression(right, rewrites);
        }
        Expression::UnaryExpression(ue) => rewrite_expression(&mut ue.expression, rewrites),
        Expression::OptionalCheckExpression(oc) => rewrite_expression(&mut oc.expression, rewrites),
        Expression::ForceUnwrapExpression(fu) => rewrite_expression(&mut fu.expression, rewrites),
        Expression::IndexExpression(ie) => {
            rewrite_expression(&mut ie.object, rewrites);
            rewrite_expression(&mut ie.index, rewrites);
        }
        Expression::ArrayExpression(ae) => {
            for item in &mut ae.items {
                rewrite_expression(item, rewrites);
            }
        }
        Expression::DictionaryExpression(de) => {
            for entry in &mut de.entries {
                rewrite_expression(&mut entry.value, rewrites);
            }
        }
        Expression::TupleExpression(te) => {
            for item in &mut te.items {
                rewrite_expression(item, rewrites);
            }
        }
        Expression::RangeExpression(re) => {
            rewrite_expression(&mut re.start, rewrites);
            rewrite_expression(&mut re.end, rewrites);
        }
        Expression::FunctionExpression(fd) => rewrite_function_body(fd, rewrites),
        Expression::TemplateStringExpression(ts) => {
            for part in &mut ts.parts {
                if let fai_compiler::ast::TemplateStringPart::Expression { expression } = part {
                    rewrite_expression(expression, rewrites);
                }
            }
        }
        Expression::IdentifierExpression(_)
        | Expression::StringExpression(_)
        | Expression::NumberExpression(_)
        | Expression::BooleanExpression(_)
        | Expression::NullExpression(_) => {}
    }
}

fn rewrite_call(call: &mut CallExpression, rewrites: &HashMap<String, String>) {
    match &mut *call.callee {
        Expression::IdentifierExpression(id) => {
            if let Some(target) = rewrites.get(&id.name) {
                id.name = target.clone();
            }
        }
        Expression::MemberExpression(MemberExpression {
            object, property, ..
        }) => {
            if let Expression::IdentifierExpression(obj) = &**object {
                if let Some(module) = rewrites.get(&obj.name) {
                    let location = call.location.clone();
                    call.callee = Box::new(Expression::IdentifierExpression(
                        fai_compiler::ast::IdentifierExpression {
                            name: format!("{}.{}", module, property),
                            location,
                        },
                    ));
                }
            } else {
                rewrite_expression(object, rewrites);
            }
        }
        other => rewrite_expression(other, rewrites),
    }
    for arg in &mut call.args {
        rewrite_expression(&mut arg.value, rewrites);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_entry(source: &str) -> Program {
        fai_compiler::prepare_source(source, None)
            .expect("entry should parse")
            .serde_ast
    }

    fn parse_module(name: &str, source: &str) -> DiscoveredModule {
        DiscoveredModule {
            name: name.to_string(),
            statements: fai_compiler::prepare_source(source, None)
                .expect("module should parse")
                .serde_ast
                .statements,
            private_names: Vec::new(),
            file_paths: Vec::new(),
        }
    }

    fn function_names(program: &Program) -> Vec<String> {
        program
            .statements
            .iter()
            .filter_map(|stmt| match stmt {
                Statement::FunctionDeclaration(fd) => Some(fd.name.clone()),
                _ => None,
            })
            .collect()
    }

    fn first_main_call_name(program: &Program) -> Option<String> {
        let main = program.statements.iter().find_map(|stmt| match stmt {
            Statement::FunctionDeclaration(fd) if fd.name == "main" => Some(fd),
            _ => None,
        })?;
        let Statement::LetStatement(bind) = main.body.first()? else {
            return None;
        };
        let Expression::CallExpression(call) = &bind.value else {
            return None;
        };
        let Expression::IdentifierExpression(callee) = &*call.callee else {
            return None;
        };
        Some(callee.name.clone())
    }

    #[test]
    fn normalizes_named_import_async_call_to_canonical_function_name() {
        let entry = parse_entry(
            "use { child } from helper\n\
             def main\n    @return Int\ndo\n  let x = child()\n  x + 1\nend\n",
        );
        let module = parse_module(
            "helper",
            "def child\n    @return Int\ndo\n  sleep(1)\n  7\nend\n",
        );

        let normalized = normalize_modules_for_legacy_async(&entry, &[module]);

        assert_eq!(
            first_main_call_name(&normalized),
            Some("helper.child".to_string())
        );
        assert!(function_names(&normalized).contains(&"helper.child".to_string()));
    }

    #[test]
    fn normalizes_namespace_async_call_to_canonical_function_name() {
        let entry = parse_entry(
            "use helper\n\
             def main\n    @return Int\ndo\n  let x = helper.child()\n  x + 1\nend\n",
        );
        let module = parse_module(
            "helper",
            "def child\n    @return Int\ndo\n  sleep(1)\n  7\nend\n",
        );

        let normalized = normalize_modules_for_legacy_async(&entry, &[module]);

        assert_eq!(
            first_main_call_name(&normalized),
            Some("helper.child".to_string())
        );
        assert!(function_names(&normalized).contains(&"helper.child".to_string()));
    }

    #[test]
    fn async_backend_compiles_normalized_module_auto_wait_shape() {
        let entry = parse_entry(
            "use { child } from helper\n\
             def main\n    @return Int\ndo\n  let x = child()\n  x + 1\nend\n",
        );
        let module = parse_module(
            "helper",
            "def child\n    @return Int\ndo\n  sleep(1)\n  7\nend\n",
        );
        let analysis = crate::async_analysis::analyze(&entry, std::slice::from_ref(&module));

        let outcome = try_codegen_async(&entry, &[module], &analysis, false)
            .expect("async backend should handle async program");

        assert!(matches!(outcome, AsyncBuildOutcome::Compiled(_)));
    }
}
