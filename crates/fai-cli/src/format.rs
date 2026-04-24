//! FAI source code formatter.
//!
//! Parses .fai files and pretty-prints them back in canonical form.

use fai_parser::ast::*;

pub struct FormatResult {
    pub file_path: String,
    pub changed: bool,
}

/// Format a file or directory. Returns results for each file processed.
pub fn format_path(target: &str, check: bool) -> Result<Vec<FormatResult>, String> {
    let path = std::path::Path::new(target);
    let files = if path.is_dir() {
        collect_fai_files(target)?
    } else {
        vec![target.to_string()]
    };

    let mut results = Vec::new();
    for file_path in files {
        let result = format_file(&file_path, check)?;
        results.push(result);
    }
    Ok(results)
}

fn format_file(file_path: &str, check: bool) -> Result<FormatResult, String> {
    let source = std::fs::read_to_string(file_path)
        .map_err(|e| format!("error reading {}: {}", file_path, e))?;
    let program = fai_parser::parse(&source)?;
    let formatted = format_program(&program);
    let changed = formatted != source;

    if changed && !check {
        std::fs::write(file_path, &formatted)
            .map_err(|e| format!("error writing {}: {}", file_path, e))?;
    }

    Ok(FormatResult {
        file_path: file_path.to_string(),
        changed,
    })
}

fn collect_fai_files(dir: &str) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    collect_fai_files_recursive(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_fai_files_recursive(dir: &str, files: &mut Vec<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("error reading {}: {}", dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("error reading entry: {}", e))?;
        let path = entry.path();
        if path.is_dir() {
            collect_fai_files_recursive(&path.to_string_lossy(), files)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("fai") {
            files.push(path.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

// ── Formatting ───────────────────────────────────────────────────────

fn is_stmt_private(stmt: &Statement) -> bool {
    match stmt {
        Statement::Function(f) => f.is_private,
        Statement::Let(l) => l.is_private,
        Statement::Var(v) => v.is_private,
        Statement::Type(t) => t.is_private,
        Statement::Enum(e) => e.is_private,
        Statement::ExternBlock(e) => e.is_private,
        _ => false,
    }
}

fn format_program(program: &Program) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut emitted_private = false;

    // File-level leading comments (harness directive blocks,
    // copyright headers, etc.) go first as a single block.
    if !program.leading_comments.is_empty() {
        parts.push(
            program
                .leading_comments
                .iter()
                .map(|c| format!("# {}", c))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    for stmt in &program.statements {
        if is_stmt_private(stmt) && !emitted_private {
            // Emit `private:` once before the first private declaration.
            // The parser's sticky mode makes everything after it private.
            parts.push("private:".to_string());
            emitted_private = true;
        }
        parts.push(format_statement(stmt, ""));
    }

    format!("{}\n", parts.join("\n\n"))
}

fn format_statement(stmt: &Statement, indent: &str) -> String {
    match stmt {
        Statement::Use(u) => {
            if let Some(names) = &u.imported_names {
                format!(
                    "{}use {{ {} }} from {}",
                    indent,
                    names.join(", "),
                    u.module_path.join(".")
                )
            } else {
                format!("{}use {}", indent, u.module_path.join("."))
            }
        }
        Statement::Let(l) => {
            format!(
                "{}let {} = {}",
                indent,
                format_bindings(&l.bindings),
                format_expression(&l.value, indent)
            )
        }
        Statement::Var(v) => {
            format!(
                "{}var {} = {}",
                indent,
                format_bindings(&v.bindings),
                format_expression(&v.value, indent)
            )
        }
        Statement::Assignment(a) => match &a.target {
            AssignmentTarget::Variables(names) => format!(
                "{}{} = {}",
                indent,
                names.join(", "),
                format_expression(&a.value, indent)
            ),
            AssignmentTarget::Field(expr) => format!(
                "{}{} = {}",
                indent,
                format_expression(expr, indent),
                format_expression(&a.value, indent)
            ),
            AssignmentTarget::Index(expr) => format!(
                "{}{} = {}",
                indent,
                format_expression(expr, indent),
                format_expression(&a.value, indent)
            ),
        },
        Statement::Function(f) => format_function_decl(f, indent),
        Statement::Type(t) => {
            let inner = format!("{}  ", indent);
            let mut lines = Vec::new();
            let remote_prefix = if t.is_remote { "remote " } else { "" };
            lines.push(format!("{}{}type {}", indent, remote_prefix, t.name));
            for tp in &t.type_params {
                lines.push(format!("{}@type {}", inner, tp.name));
            }
            for f in &t.fields {
                lines.push(format!("{}{}", inner, format_field(f)));
            }
            lines.push(format!("{}end", indent));
            lines.join("\n")
        }
        Statement::Enum(e) => {
            let members: Vec<String> = e
                .members
                .iter()
                .map(|m| format!("{}  {}", indent, m))
                .collect();
            format!(
                "{}enum {}\n{}\n{}end",
                indent,
                e.name,
                members.join("\n"),
                indent
            )
        }
        Statement::Test(t) => format_test_decl(t, indent),
        Statement::If(i) => format_if_stmt(i, indent),
        Statement::Case(c) => format_case_stmt(c, indent),
        Statement::Try(t) => format_try_stmt(t, indent),
        Statement::Throw(t) => {
            format!(
                "{}throw {}",
                indent,
                format_expression(&t.expression, indent)
            )
        }
        Statement::Nowait(nw) => {
            format!(
                "{}nowait {}",
                indent,
                format_expression(&nw.expression, indent)
            )
        }
        Statement::For(f) => {
            format!(
                "{}for {} in {}\n{}\n{}end",
                indent,
                f.item_name,
                format_expression(&f.items, indent),
                format_block(&f.body, indent),
                indent
            )
        }
        Statement::While(w) => {
            format!(
                "{}while {}\n{}\n{}end",
                indent,
                format_expression(&w.condition, indent),
                format_block(&w.body, indent),
                indent
            )
        }
        Statement::Break(_) => format!("{}break", indent),
        Statement::Continue(_) => format!("{}continue", indent),
        Statement::Return(r) => match &r.value {
            Some(expr) => format!("{}return {}", indent, format_expression(expr, indent)),
            None => format!("{}return", indent),
        },
        Statement::Expression(e) => {
            format!("{}{}", indent, format_expression(&e.expression, indent))
        }
        Statement::ExternBlock(ext) => {
            let inner = format!("{}  ", indent);
            let mut lines = vec![format!("{}extern {}", indent, ext.library)];
            for t in &ext.types {
                lines.push(format!("{}type {}", inner, t.name));
            }
            for f in &ext.functions {
                let params: Vec<String> = f.params.iter().map(format_parameter).collect();
                let ret = f
                    .return_type
                    .as_ref()
                    .map(|t| format!(" -> {}", format_type_node(t)))
                    .unwrap_or_default();
                lines.push(format!(
                    "{}def {}({}){}",
                    inner,
                    f.name,
                    params.join(", "),
                    ret
                ));
            }
            lines.push(format!("{}end", indent));
            lines.join("\n")
        }
        Statement::FunctionTypeDef(ftd) => {
            let inner = format!("{}    ", indent);
            let mut lines = Vec::new();
            if let Some(doc) = &ftd.doc_comment {
                for line in doc.lines() {
                    lines.push(format!("{}# {}", indent, line));
                }
            }
            lines.push(format!("{}type def {}", indent, ftd.name));
            for p in &ftd.params {
                if let Some(doc) = &p.doc_comment {
                    for line in doc.lines() {
                        lines.push(format!("{}# {}", inner, line));
                    }
                }
                let mutable_text = if p.is_mutable { ", mutable" } else { "" };
                let default_text = match &p.default_value {
                    Some(expr) => format!(", default: {}", format_expression(expr, "")),
                    None => String::new(),
                };
                lines.push(format!(
                    "{}@param {} {}{}{}",
                    inner,
                    p.name,
                    format_type_node(&p.type_node),
                    mutable_text,
                    default_text
                ));
            }
            for r in &ftd.return_types {
                lines.push(format!(
                    "{}@return {}",
                    inner,
                    format_type_node(&r.type_node)
                ));
            }
            lines.push(format!("{}end", indent));
            lines.join("\n")
        }
    }
}

fn format_function_decl(f: &FunctionDeclaration, indent: &str) -> String {
    let is_synthetic = f.name.starts_with('<');

    // Synthetic functions (anonymous blocks, closures) use old format
    if is_synthetic {
        return format_function_decl_old(f, indent);
    }

    // Named functions use new v2 format
    format_function_decl_v2(f, indent)
}

/// Format a function in old inline syntax (for anonymous/synthetic functions).
fn format_function_decl_old(f: &FunctionDeclaration, indent: &str) -> String {
    let params: Vec<String> = f.params.iter().map(format_parameter).collect();
    let returns_void = f.return_types.len() == 1
        && f.return_types[0].type_node.name.as_deref() == Some("Void")
        && !f.return_types[0].type_node.is_array
        && !f.return_types[0].type_node.is_optional
        && f.return_types[0].type_node.function_params.is_none();

    let return_text = if returns_void || f.return_types.is_empty() {
        String::new()
    } else {
        let types: Vec<String> = f
            .return_types
            .iter()
            .map(|r| format_type_node(&r.type_node))
            .collect();
        format!(" -> {}", types.join(", "))
    };

    let tp = if f.type_params.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = f.type_params.iter().map(|tp| tp.name.as_str()).collect();
        format!("<{}>", names.join(", "))
    };

    format!(
        "{}def {}{}({}){}\n{}\n{}end",
        indent,
        f.name,
        tp,
        params.join(", "),
        return_text,
        format_block(&f.body, indent),
        indent
    )
}

/// Format a function in new v2 syntax with @type/@param/@return and do...end.
fn format_function_decl_v2(f: &FunctionDeclaration, indent: &str) -> String {
    let inner = format!("{}    ", indent);
    let mut lines = Vec::new();

    // Doc comment
    if let Some(doc) = &f.doc_comment {
        for line in doc.lines() {
            lines.push(format!("{}# {}", indent, line));
        }
    }

    // def name (with remote prefix if applicable)
    let remote_prefix = if f.is_remote { "remote " } else { "" };
    lines.push(format!("{}{}def {}", indent, remote_prefix, f.name));

    // @type declarations
    for tp in &f.type_params {
        if let Some(doc) = &tp.doc_comment {
            for line in doc.lines() {
                lines.push(format!("{}# {}", inner, line));
            }
        }
        lines.push(format!("{}@type {}", inner, tp.name));
    }

    // @param declarations
    for p in &f.params {
        if let Some(doc) = &p.doc_comment {
            for line in doc.lines() {
                lines.push(format!("{}# {}", inner, line));
            }
        }
        let mutable_text = if p.is_mutable { ", mutable" } else { "" };
        let default_text = match &p.default_value {
            Some(expr) => format!(", default: {}", format_expression(expr, "")),
            None => String::new(),
        };
        lines.push(format!(
            "{}@param {} {}{}{}",
            inner,
            p.name,
            format_type_node(&p.type_node),
            mutable_text,
            default_text
        ));
    }

    // @return declarations
    for r in &f.return_types {
        if let Some(doc) = &r.doc_comment {
            for line in doc.lines() {
                lines.push(format!("{}# {}", inner, line));
            }
        }
        let name_text = match &r.name {
            Some(name) => format!("{} ", name),
            None => String::new(),
        };
        lines.push(format!(
            "{}@return {}{}",
            inner,
            name_text,
            format_type_node(&r.type_node)
        ));
    }

    // do ... body ... end
    lines.push(format!("{}do", indent));
    lines.push(format_block(&f.body, indent));
    lines.push(format!("{}end", indent));

    lines.join("\n")
}

fn format_test_decl(t: &TestDeclaration, indent: &str) -> String {
    let mut parts = vec![format!("{}test {}", indent, t.name)];

    if !t.setup.is_empty() {
        parts.push(format_block(&t.setup, indent));
    }
    if let Some(ba) = &t.before_all {
        parts.push(format!(
            "{}beforeAll\n{}\n{}end",
            indent,
            format_block(ba, indent),
            indent
        ));
    }
    if let Some(be) = &t.before_each {
        parts.push(format!(
            "{}beforeEach\n{}\n{}end",
            indent,
            format_block(be, indent),
            indent
        ));
    }
    for case in &t.cases {
        parts.push(format!(
            "{}it '{}'\n{}\n{}end",
            indent,
            case.description,
            format_block(&case.body, indent),
            indent
        ));
    }
    if let Some(ae) = &t.after_each {
        parts.push(format!(
            "{}afterEach\n{}\n{}end",
            indent,
            format_block(ae, indent),
            indent
        ));
    }
    if let Some(aa) = &t.after_all {
        parts.push(format!(
            "{}afterAll\n{}\n{}end",
            indent,
            format_block(aa, indent),
            indent
        ));
    }
    parts.push(format!("{}end", indent));
    parts.join("\n")
}

fn format_if_stmt(i: &IfStatement, indent: &str) -> String {
    let mut parts = Vec::new();
    for (idx, branch) in i.branches.iter().enumerate() {
        let prefix = if idx == 0 { "if" } else { "else if" };
        parts.push(format!(
            "{}{} {}\n{}",
            indent,
            prefix,
            format_expression(&branch.condition, indent),
            format_block(&branch.body, indent)
        ));
    }
    if let Some(else_body) = &i.else_branch {
        parts.push(format!(
            "{}else\n{}",
            indent,
            format_block(else_body, indent)
        ));
    }
    parts.push(format!("{}end", indent));
    parts.join("\n")
}

fn format_case_stmt(c: &CaseStatement, indent: &str) -> String {
    let mut parts = vec![format!(
        "{}case {}",
        indent,
        format_expression(&c.value, indent)
    )];
    for branch in &c.when_branches {
        parts.push(format!(
            "{}when {}\n{}",
            indent,
            format_expression(&branch.match_expr, indent),
            format_block(&branch.body, indent)
        ));
    }
    if let Some(default_body) = &c.default_branch {
        parts.push(format!(
            "{}default\n{}",
            indent,
            format_block(default_body, indent)
        ));
    }
    parts.push(format!("{}end", indent));
    parts.join("\n")
}

fn format_try_stmt(t: &TryStatement, indent: &str) -> String {
    let mut s = format!(
        "{}try\n{}\n{}catch {}\n{}",
        indent,
        format_block(&t.try_body, indent),
        indent,
        t.catch_name,
        format_block(&t.catch_body, indent)
    );
    if let Some(finally_body) = &t.finally_body {
        s.push_str(&format!(
            "\n{}finally\n{}",
            indent,
            format_block(finally_body, indent)
        ));
    }
    s.push_str(&format!("\n{}end", indent));
    s
}

fn format_block(statements: &[Statement], indent: &str) -> String {
    let next_indent = format!("{}  ", indent);
    statements
        .iter()
        .map(|s| format_statement(s, &next_indent))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_bindings(bindings: &[BindingDeclaration]) -> String {
    bindings
        .iter()
        .map(|b| {
            if let Some(tn) = &b.type_name {
                format!("{} {}", b.name, format_type_node(tn))
            } else {
                b.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_parameter(p: &Parameter) -> String {
    let default_text = match &p.default_value {
        Some(expr) => format!(" = {}", format_expression(expr, "")),
        None => String::new(),
    };
    // Preserve `out` keyword for extern function output pointer parameters
    let out_prefix = if p.is_out { "out " } else { "" };
    format!(
        "{}{}: {}{}",
        out_prefix,
        p.name,
        format_type_node(&p.type_node),
        default_text
    )
}

fn format_field(f: &FieldDeclaration) -> String {
    let default_text = match &f.default_value {
        Some(expr) => format!(" = {}", format_expression(expr, "")),
        None => String::new(),
    };
    format!(
        "{} {}{}",
        f.name,
        format_type_node(&f.type_node),
        default_text
    )
}

fn format_type_node(tn: &TypeNode) -> String {
    let mut text = if tn.function_params.is_some() && tn.function_returns.is_some() {
        let params: Vec<String> = tn
            .function_params
            .as_ref()
            .unwrap()
            .iter()
            .map(format_type_node)
            .collect();
        let returns: Vec<String> = tn
            .function_returns
            .as_ref()
            .unwrap()
            .iter()
            .map(format_type_node)
            .collect();
        format!("({}) -> {}", params.join(", "), returns.join(", "))
    } else {
        let prefix = if tn.is_type_parameter { "$" } else { "" };
        format!("{}{}", prefix, tn.name.as_deref().unwrap_or("Unknown"))
    };
    if tn.is_array {
        text.push_str("[]");
    }
    if tn.is_optional {
        text.push('?');
    }
    text
}

// ── Expression formatting ────────────────────────────────────────────

fn format_expression(expr: &Expression, indent: &str) -> String {
    match expr {
        Expression::Identifier(e) => e.name.clone(),
        Expression::String(e) => format!("'{}'", escape_string_contents(&e.value, '\'')),
        Expression::TemplateString(ts) => {
            let raw: String = ts
                .parts
                .iter()
                .map(|p| match p {
                    TemplateStringPart::Text(t) => escape_string_contents(t, '"'),
                    TemplateStringPart::Expr(e) => {
                        format!("{{{{{}}}}}", format_expression(e, indent))
                    }
                })
                .collect();
            format!("\"{}\"", raw)
        }
        Expression::Number(e) => {
            // Preserve the source syntactic form — whole-valued floats
            // like `1.0` must not collapse to `1`, because `is_float`
            // drives the inferred type (Int vs Float).
            if e.is_float {
                if e.value == (e.value as i64) as f64 {
                    format!("{}.0", e.value as i64)
                } else {
                    format!("{}", e.value)
                }
            } else {
                format!("{}", e.value as i64)
            }
        }
        Expression::Boolean(e) => {
            if e.value {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Expression::Null(_) => "null".to_string(),
        Expression::Array(a) => {
            if a.items.is_empty() {
                "[]".to_string()
            } else {
                match a.style {
                    ArrayLiteralStyle::Inline => {
                        let items: Vec<String> = a
                            .items
                            .iter()
                            .map(|i| format_expression(i, indent))
                            .collect();
                        format!("[{}]", items.join(" "))
                    }
                    ArrayLiteralStyle::Vertical => {
                        let next_indent = format!("{}  ", indent);
                        let needs_commas = a
                            .items
                            .iter()
                            .any(|item| matches!(item, Expression::Array(_)));
                        let items: Vec<String> = a
                            .items
                            .iter()
                            .map(|i| {
                                format!("{}{}", next_indent, format_expression(i, &next_indent))
                            })
                            .collect();
                        let joiner = if needs_commas { ",\n" } else { "\n" };
                        format!("[\n{}\n{}]", items.join(joiner), indent)
                    }
                }
            }
        }
        Expression::Dictionary(d) => {
            if d.entries.is_empty() {
                "{}".to_string()
            } else {
                let next_indent = format!("{}  ", indent);
                let entries: Vec<String> = d
                    .entries
                    .iter()
                    .map(|e| {
                        format!(
                            "{}{}: {}",
                            next_indent,
                            e.key,
                            format_expression(&e.value, &next_indent)
                        )
                    })
                    .collect();
                format!("{{\n{}\n{}}}", entries.join("\n"), indent)
            }
        }
        Expression::Tuple(t) => {
            let items: Vec<String> = t
                .items
                .iter()
                .map(|i| format_expression(i, indent))
                .collect();
            items.join(", ")
        }
        Expression::Range(r) => {
            let op = if r.inclusive { "..." } else { ".." };
            format!(
                "{}{}{}",
                format_expression(&r.start, indent),
                op,
                format_expression(&r.end, indent)
            )
        }
        Expression::Call(c) => {
            // Detect trailing closure: last unlabeled arg is a synthetic do...end block
            let has_trailing = c.args.last().map(|a| {
                a.label.is_none()
                    && matches!(&a.value, Expression::Function(f) if f.name.starts_with("<block:"))
            }).unwrap_or(false);

            if has_trailing {
                let n = c.args.len() - 1;
                let regular_args: Vec<String> = c.args[..n]
                    .iter()
                    .map(|a| {
                        if let Some(label) = &a.label {
                            format!("{}: {}", label, format_expression(&a.value, indent))
                        } else {
                            format_expression(&a.value, indent)
                        }
                    })
                    .collect();
                let closure = match &c.args[n].value {
                    Expression::Function(f) => f,
                    _ => unreachable!(),
                };
                let args_str = if regular_args.is_empty() {
                    String::new()
                } else {
                    format!("({})", regular_args.join(", "))
                };
                format!(
                    "{}{} {}",
                    format_expression(&c.callee, indent),
                    args_str,
                    format_do_block(closure, indent)
                )
            } else {
                let args: Vec<String> = c
                    .args
                    .iter()
                    .map(|a| {
                        if let Some(label) = &a.label {
                            format!("{}: {}", label, format_expression(&a.value, indent))
                        } else {
                            format_expression(&a.value, indent)
                        }
                    })
                    .collect();
                format!(
                    "{}({})",
                    format_expression(&c.callee, indent),
                    args.join(", ")
                )
            }
        }
        Expression::Member(m) => {
            format!("{}.{}", format_expression(&m.object, indent), m.property)
        }
        Expression::Unary(u) => {
            // Keyword operators (`not`) need a space before the operand;
            // symbol operators (`!`, `-`) sit flush against it.
            let sep = if u
                .operator
                .chars()
                .next()
                .map_or(false, |c| c.is_alphabetic())
            {
                " "
            } else {
                ""
            };
            format!(
                "{}{}{}",
                u.operator,
                sep,
                format_expression(&u.expression, indent)
            )
        }
        Expression::OptionalCheck(inner, _) => {
            format!("{}?", format_expression(inner, indent))
        }
        Expression::ForceUnwrap(inner, _) => {
            format!("{}!", format_expression(inner, indent))
        }
        Expression::Binary(b) => {
            format!(
                "{} {} {}",
                format_expression(&b.left, indent),
                b.operator,
                format_expression(&b.right, indent)
            )
        }
        Expression::Index(ix) => {
            format!(
                "{}[{}]",
                format_expression(&ix.object, indent),
                format_expression(&ix.index, indent)
            )
        }
        Expression::Function(f) => {
            if f.name.starts_with("<block:") {
                // Synthetic do...end closure: emit as `do [with params] body end`
                format_do_block(f, indent)
            } else {
                format_function_decl(f, indent)
            }
        }
    }
}

fn escape_string_contents(value: &str, quote: char) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            '\'' if quote == '\'' => out.push_str("\\'"),
            '"' if quote == '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out
}

/// Format a synthetic `do...end` closure back to surface syntax.
fn format_do_block(f: &FunctionDeclaration, indent: &str) -> String {
    let params_str = if f.params.is_empty() {
        String::new()
    } else {
        let params: Vec<String> = f
            .params
            .iter()
            .map(|p| format!("{} {}", p.name, format_type_node(&p.type_node)))
            .collect();
        format!(" with {}", params.join(", "))
    };
    let inner = format!("{}  ", indent);
    format!(
        "do{}\n{}\n{}end",
        params_str,
        format_block(&f.body, &inner),
        indent
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_parser::ast::*;

    // Round-trip helper: parse FAI source and re-format it.
    fn rt(src: &str) -> String {
        let program =
            fai_parser::parse(src).unwrap_or_else(|e| panic!("parse failed for {:?}: {}", src, e));
        format_program(&program)
    }

    // Build a minimal SourceLocation.
    fn loc() -> SourceLocation {
        SourceLocation { line: 1, column: 1 }
    }

    // Build a simple named TypeNode.
    fn ty(name: &str) -> TypeNode {
        TypeNode {
            name: Some(name.to_string()),
            is_type_parameter: false,
            function_params: None,
            function_returns: None,
            is_array: false,
            is_optional: false,
            location: loc(),
        }
    }

    fn ident(name: &str) -> Expression {
        Expression::Identifier(IdentifierExpr {
            name: name.to_string(),
            location: loc(),
        })
    }

    fn num(v: f64) -> Expression {
        Expression::Number(NumberExpr {
            value: v,
            is_float: false,
            location: loc(),
        })
    }

    fn expr_stmt(e: Expression) -> Statement {
        Statement::Expression(ExpressionStatement {
            expression: e,
            location: loc(),
        })
    }

    fn make_program(stmts: Vec<Statement>) -> Program {
        Program {
            statements: stmts,
            leading_comments: Vec::new(),
        }
    }

    // ── Statement round-trips ────────────────────────────────────────

    #[test]
    fn test_format_let_number() {
        assert_eq!(rt("let x = 42\n"), "let x = 42\n");
    }

    #[test]
    fn test_format_var_number() {
        assert_eq!(rt("var count = 0\n"), "var count = 0\n");
    }

    #[test]
    fn test_format_let_string() {
        assert_eq!(rt("let s = 'hello'\n"), "let s = 'hello'\n");
    }

    #[test]
    fn test_format_let_bool() {
        assert_eq!(rt("let a = true\n"), "let a = true\n");
        assert_eq!(rt("let b = false\n"), "let b = false\n");
    }

    #[test]
    fn test_format_let_null() {
        assert_eq!(rt("let x = null\n"), "let x = null\n");
    }

    #[test]
    fn test_format_use_simple() {
        assert_eq!(rt("use std.array\n"), "use std.array\n");
    }

    #[test]
    fn test_format_use_with_names() {
        assert_eq!(
            rt("use { foo, bar } from mymod.utils\n"),
            "use { foo, bar } from mymod.utils\n"
        );
    }

    #[test]
    fn test_format_assignment_variable() {
        assert_eq!(rt("x = 42\n"), "x = 42\n");
    }

    #[test]
    fn test_format_if_else() {
        let src = "if x > 0\n  print('pos')\nelse\n  print('neg')\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_if_only() {
        let src = "if x > 0\n  print('pos')\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_if_else_if() {
        let src =
            "if x > 0\n  print('pos')\nelse if x < 0\n  print('neg')\nelse\n  print('zero')\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_for_range() {
        let src = "for i in 0..9\n  print(i)\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_while() {
        let src = "while x > 0\n  x = x - 1\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_case_when_default() {
        let src = "case x\nwhen 1\n  print('one')\ndefault\n  print('other')\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_case_no_default() {
        let src = "case x\nwhen 1\n  print('one')\nwhen 2\n  print('two')\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_try_catch() {
        let src = "try\n  risky()\ncatch e\n  print(e)\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_try_catch_finally() {
        let src = "try\n  risky()\ncatch e\n  print(e)\nfinally\n  cleanup()\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_throw() {
        assert_eq!(rt("throw e\n"), "throw e\n");
    }

    #[test]
    fn test_format_nowait() {
        assert_eq!(rt("nowait task()\n"), "nowait task()\n");
    }

    #[test]
    fn test_format_break() {
        assert_eq!(
            rt("while true\n  break\nend\n"),
            "while true\n  break\nend\n"
        );
    }

    #[test]
    fn test_format_continue() {
        assert_eq!(
            rt("while true\n  continue\nend\n"),
            "while true\n  continue\nend\n"
        );
    }

    #[test]
    fn test_format_expression_call() {
        assert_eq!(rt("print('hello')\n"), "print('hello')\n");
    }

    #[test]
    fn test_format_type_declaration() {
        let src = "type Point\n  x Int\n  y Int\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_type_with_type_params() {
        // The formatter prefixes type-parameter references with `$`
        let src = "type Box\n  @type T\n  value T\nend\n";
        let formatted = rt(src);
        assert!(formatted.contains("@type T"), "should have @type T");
        assert!(
            formatted.contains("$T"),
            "should have $T for type param ref"
        );
    }

    #[test]
    fn test_format_enum() {
        let src = "enum Color\n  red\n  green\n  blue\nend\n";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_multiple_statements() {
        let src = "let x = 1\n\nlet y = 2\n";
        assert_eq!(rt(src), src);
    }

    // ── Expression formatting ────────────────────────────────────────

    #[test]
    fn test_format_expr_binary() {
        assert_eq!(rt("let r = a + b\n"), "let r = a + b\n");
    }

    #[test]
    fn test_format_expr_unary_neg() {
        assert_eq!(rt("let r = -x\n"), "let r = -x\n");
    }

    #[test]
    fn test_format_expr_member() {
        assert_eq!(rt("let n = obj.name\n"), "let n = obj.name\n");
    }

    #[test]
    fn test_format_expr_index() {
        assert_eq!(rt("let v = arr[0]\n"), "let v = arr[0]\n");
    }

    #[test]
    fn test_format_expr_range() {
        assert_eq!(rt("let r = 0..9\n"), "let r = 0..9\n");
    }

    #[test]
    fn test_format_expr_empty_array() {
        assert_eq!(rt("let a = []\n"), "let a = []\n");
    }

    #[test]
    fn test_format_expr_empty_dict() {
        assert_eq!(rt("let d = {}\n"), "let d = {}\n");
    }

    #[test]
    fn test_format_expr_call_with_label() {
        assert_eq!(rt("f(x: 1, y: 2)\n"), "f(x: 1, y: 2)\n");
    }

    #[test]
    fn test_format_expr_optional_check() {
        assert_eq!(rt("let ok = x?\n"), "let ok = x?\n");
    }

    #[test]
    fn test_format_expr_force_unwrap() {
        assert_eq!(rt("let v = x!\n"), "let v = x!\n");
    }

    #[test]
    fn test_format_float_number() {
        assert_eq!(rt("let pi = 3.14\n"), "let pi = 3.14\n");
    }

    #[test]
    fn test_format_preserves_whole_float() {
        // Whole-valued floats must keep their `.0` so `is_float` is
        // preserved — the checker uses it to decide whether a literal
        // narrows cleanly to Int. Collapsing `1.0` to `1` would silently
        // change a binding's inferred type.
        assert_eq!(rt("let v = 1.0\n"), "let v = 1.0\n");
        assert_eq!(rt("let v Float = 1.0\n"), "let v Float = 1.0\n");
    }

    #[test]
    fn test_format_preserves_int() {
        assert_eq!(rt("let v = 1\n"), "let v = 1\n");
        assert_eq!(rt("let v Int = 1\n"), "let v Int = 1\n");
    }

    // ── TypeNode formatting ──────────────────────────────────────────

    #[test]
    fn test_format_type_node_simple() {
        let tn = ty("String");
        assert_eq!(format_type_node(&tn), "String");
    }

    #[test]
    fn test_format_type_node_array() {
        let tn = TypeNode {
            is_array: true,
            ..ty("Int")
        };
        assert_eq!(format_type_node(&tn), "Int[]");
    }

    #[test]
    fn test_format_type_node_optional() {
        let tn = TypeNode {
            is_optional: true,
            ..ty("String")
        };
        assert_eq!(format_type_node(&tn), "String?");
    }

    #[test]
    fn test_format_type_node_array_optional() {
        let tn = TypeNode {
            is_array: true,
            is_optional: true,
            ..ty("Int")
        };
        assert_eq!(format_type_node(&tn), "Int[]?");
    }

    #[test]
    fn test_format_type_node_function_type() {
        let tn = TypeNode {
            name: None,
            is_type_parameter: false,
            function_params: Some(vec![ty("Int"), ty("String")]),
            function_returns: Some(vec![ty("Bool")]),
            is_array: false,
            is_optional: false,
            location: loc(),
        };
        assert_eq!(format_type_node(&tn), "(Int, String) -> Bool");
    }

    #[test]
    fn test_format_type_node_type_parameter() {
        let tn = TypeNode {
            is_type_parameter: true,
            ..ty("T")
        };
        assert_eq!(format_type_node(&tn), "$T");
    }

    // ── format_bindings ──────────────────────────────────────────────

    #[test]
    fn test_format_bindings_no_type() {
        let b = vec![BindingDeclaration {
            name: "x".to_string(),
            type_name: None,
        }];
        assert_eq!(format_bindings(&b), "x");
    }

    #[test]
    fn test_format_bindings_with_type() {
        let b = vec![BindingDeclaration {
            name: "x".to_string(),
            type_name: Some(ty("Int")),
        }];
        assert_eq!(format_bindings(&b), "x Int");
    }

    #[test]
    fn test_format_bindings_multiple() {
        let b = vec![
            BindingDeclaration {
                name: "a".to_string(),
                type_name: None,
            },
            BindingDeclaration {
                name: "b".to_string(),
                type_name: None,
            },
        ];
        assert_eq!(format_bindings(&b), "a, b");
    }

    // ── format_parameter ────────────────────────────────────────────

    #[test]
    fn test_format_parameter_no_default() {
        let p = Parameter {
            name: "x".to_string(),
            type_node: ty("Int"),
            default_value: None,
            is_out: false,
            is_mutable: false,
            location: loc(),
            doc_comment: None,
        };
        assert_eq!(format_parameter(&p), "x: Int");
    }

    #[test]
    fn test_format_parameter_with_default() {
        let p = Parameter {
            name: "x".to_string(),
            type_node: ty("Int"),
            default_value: Some(num(0.0)),
            is_out: false,
            is_mutable: false,
            location: loc(),
            doc_comment: None,
        };
        assert_eq!(format_parameter(&p), "x: Int = 0");
    }

    // ── format_program (direct construction) ────────────────────────

    #[test]
    fn test_format_program_single_stmt() {
        let prog = make_program(vec![expr_stmt(ident("x"))]);
        assert_eq!(format_program(&prog), "x\n");
    }

    #[test]
    fn test_format_program_two_stmts() {
        let prog = make_program(vec![expr_stmt(ident("a")), expr_stmt(ident("b"))]);
        assert_eq!(format_program(&prog), "a\n\nb\n");
    }

    #[test]
    fn test_format_program_empty() {
        let prog = make_program(vec![]);
        assert_eq!(format_program(&prog), "\n");
    }

    // ── format_path I/O ──────────────────────────────────────────────

    #[test]
    fn test_format_path_already_formatted() {
        let dir = std::env::temp_dir().join("fai_fmt_test_already");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.fai");
        let src = "let x = 42\n";
        std::fs::write(&path, src).unwrap();

        let results = format_path(path.to_str().unwrap(), false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].changed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_format_path_check_mode_unchanged() {
        let dir = std::env::temp_dir().join("fai_fmt_test_check");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.fai");
        std::fs::write(&path, "let x = 42\n").unwrap();

        let results = format_path(path.to_str().unwrap(), true).unwrap();
        assert!(!results[0].changed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_format_path_directory() {
        let dir = std::env::temp_dir().join("fai_fmt_test_dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.fai"), "let x = 1\n").unwrap();
        std::fs::write(dir.join("b.fai"), "let y = 2\n").unwrap();
        std::fs::write(dir.join("other.txt"), "not fai").unwrap();

        let results = format_path(dir.to_str().unwrap(), false).unwrap();
        assert_eq!(results.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_format_path_nonexistent_returns_error() {
        let result = format_path("/nonexistent/path/file.fai", false);
        assert!(result.is_err());
    }

    // ── format_file write path ───────────────────────────────────────

    #[test]
    fn test_format_path_writes_changed_file() {
        let dir = std::env::temp_dir().join("fai_fmt_writes");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.fai");
        // No trailing newline — formatter will add one
        std::fs::write(&path, "let x = 42").unwrap();

        let results = format_path(path.to_str().unwrap(), false).unwrap();
        assert!(results[0].changed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "let x = 42\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Assignment targets Field and Index ───────────────────────────

    #[test]
    fn test_format_assignment_field() {
        assert_eq!(rt("obj.field = 42\n"), "obj.field = 42\n");
    }

    #[test]
    fn test_format_assignment_index() {
        assert_eq!(rt("arr[0] = 42\n"), "arr[0] = 42\n");
    }

    // ── Function declarations ────────────────────────────────────────

    #[test]
    fn test_format_remote_def_preserved() {
        // remote def must survive a round-trip through the formatter.
        let src = "\
# Fetch the count.
remote def getCounter
    @return Int
do

end
";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_remote_type_preserved() {
        // remote type must survive a round-trip through the formatter.
        let src = "\
remote type Task
  id Int
  text String
  done Bool
end
";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_function_decl_v2_no_body() {
        let l = loc();
        let f = FunctionDeclaration {
            name: "greet".to_string(),
            type_params: vec![],
            params: vec![Parameter {
                name: "name".to_string(),
                type_node: ty("String"),
                default_value: None,
                is_out: false,
                is_mutable: false,
                location: l.clone(),
                doc_comment: None,
            }],
            return_types: vec![ReturnDeclaration {
                name: None,
                type_node: ty("Void"),
                doc_comment: None,
                location: l.clone(),
            }],
            body: vec![],
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: l,
            doc_comment: Some("Greets someone.".to_string()),
        };
        let result = format_function_decl(&f, "");
        assert!(result.contains("# Greets someone."));
        assert!(result.contains("def greet"));
        assert!(result.contains("@param name String"));
        assert!(result.contains("@return Void"));
        assert!(result.contains("do"));
        assert!(result.ends_with("end"));
    }

    #[test]
    fn test_format_function_decl_v2_with_type_param() {
        let l = loc();
        let mut tp = TypeParamDeclaration {
            name: "T".to_string(),
            doc_comment: Some("The value type.".to_string()),
            location: l.clone(),
        };
        let f = FunctionDeclaration {
            name: "identity".to_string(),
            type_params: vec![tp],
            params: vec![],
            return_types: vec![ReturnDeclaration {
                name: Some("result".to_string()),
                type_node: ty("T"),
                doc_comment: Some("The result.".to_string()),
                location: l.clone(),
            }],
            body: vec![],
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: l,
            doc_comment: Some("Identity function.".to_string()),
        };
        let result = format_function_decl(&f, "");
        assert!(result.contains("@type T"));
        assert!(result.contains("# The value type."));
        assert!(result.contains("# The result."));
        assert!(result.contains("result T"));
    }

    #[test]
    fn test_format_function_decl_v2_with_default_param() {
        let l = loc();
        let f = FunctionDeclaration {
            name: "greet".to_string(),
            type_params: vec![],
            params: vec![Parameter {
                name: "name".to_string(),
                type_node: ty("String"),
                default_value: Some(Expression::String(StringExpr {
                    value: "world".to_string(),
                    location: l.clone(),
                })),
                is_out: false,
                is_mutable: false,
                location: l.clone(),
                doc_comment: Some("The name.".to_string()),
            }],
            return_types: vec![],
            body: vec![],
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: l,
            doc_comment: None,
        };
        let result = format_function_decl(&f, "");
        assert!(result.contains("# The name."));
        assert!(result.contains("@param name String, default: 'world'"));
    }

    #[test]
    fn test_format_mutable_param_preserved() {
        // Round-trip: mutable modifier must survive fmt
        let src = "\
# Set the value in place.
def setValue
    @param signal Signal, mutable
    @param newValue Int
    @return Void
do

end
";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_mutable_param_with_default_preserved() {
        // mutable + default together
        let src = "\
# Do a thing.
def example
    @param x Int, mutable, default: 0
    @return Void
do

end
";
        assert_eq!(rt(src), src);
    }

    #[test]
    fn test_format_type_def_mutable_param_preserved() {
        // Parser doesn't yet produce mutable type def params, so test via direct AST.
        let l = loc();
        let stmt = Statement::FunctionTypeDef(FunctionTypeDefDeclaration {
            name: "Mutator".to_string(),
            type_params: vec![],
            params: vec![Parameter {
                name: "signal".to_string(),
                type_node: ty("Signal"),
                default_value: None,
                is_out: false,
                is_mutable: true,
                location: l.clone(),
                doc_comment: None,
            }],
            return_types: vec![ReturnDeclaration {
                name: None,
                type_node: ty("Void"),
                doc_comment: None,
                location: l.clone(),
            }],
            is_private: false,
            doc_comment: None,
            location: l,
        });
        let result = format_program(&make_program(vec![stmt]));
        assert!(
            result.contains("@param signal Signal, mutable"),
            "got: {}",
            result
        );
    }

    #[test]
    fn test_format_type_def_default_param_preserved() {
        // Parser doesn't yet produce type def params with defaults, so test via direct AST.
        let l = loc();
        let stmt = Statement::FunctionTypeDef(FunctionTypeDefDeclaration {
            name: "Handler".to_string(),
            type_params: vec![],
            params: vec![Parameter {
                name: "name".to_string(),
                type_node: ty("String"),
                default_value: Some(Expression::String(StringExpr {
                    value: "world".to_string(),
                    location: l.clone(),
                })),
                is_out: false,
                is_mutable: false,
                location: l.clone(),
                doc_comment: None,
            }],
            return_types: vec![ReturnDeclaration {
                name: None,
                type_node: ty("Void"),
                doc_comment: None,
                location: l.clone(),
            }],
            is_private: false,
            doc_comment: None,
            location: l,
        });
        let result = format_program(&make_program(vec![stmt]));
        assert!(
            result.contains("@param name String, default: 'world'"),
            "got: {}",
            result
        );
    }

    #[test]
    fn test_format_function_decl_old_synthetic() {
        let l = loc();
        let f = FunctionDeclaration {
            name: "<anon>".to_string(),
            type_params: vec![TypeParamDeclaration {
                name: "T".to_string(),
                doc_comment: None,
                location: l.clone(),
            }],
            params: vec![Parameter {
                name: "x".to_string(),
                type_node: ty("Int"),
                default_value: None,
                is_out: false,
                is_mutable: false,
                location: l.clone(),
                doc_comment: None,
            }],
            return_types: vec![ReturnDeclaration {
                name: None,
                type_node: ty("Int"),
                doc_comment: None,
                location: l.clone(),
            }],
            body: vec![],
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: l,
            doc_comment: None,
        };
        let result = format_function_decl(&f, "");
        // Old-style: def <anon><T>(x: Int) -> Int
        assert!(result.starts_with("def <anon>"));
        assert!(result.contains("(x: Int)"));
        assert!(result.contains("-> Int"));
    }

    #[test]
    fn test_format_function_decl_old_returns_void() {
        let l = loc();
        let f = FunctionDeclaration {
            name: "<block>".to_string(),
            type_params: vec![],
            params: vec![],
            return_types: vec![ReturnDeclaration {
                name: None,
                type_node: ty("Void"),
                doc_comment: None,
                location: l.clone(),
            }],
            body: vec![],
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: l,
            doc_comment: None,
        };
        let result = format_function_decl(&f, "");
        // Void return should be suppressed in old-style
        assert!(!result.contains("->"));
    }

    #[test]
    fn test_format_function_as_statement() {
        let l = loc();
        let f = FunctionDeclaration {
            name: "compute".to_string(),
            type_params: vec![],
            params: vec![],
            return_types: vec![ReturnDeclaration {
                name: None,
                type_node: ty("Int"),
                doc_comment: None,
                location: l.clone(),
            }],
            body: vec![expr_stmt(num(42.0))],
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: l,
            doc_comment: Some("Compute something.".to_string()),
        };
        let prog = make_program(vec![Statement::Function(f)]);
        let result = format_program(&prog);
        assert!(result.contains("def compute"));
        assert!(result.contains("@return Int"));
        assert!(result.contains("  42"));
    }

    #[test]
    fn test_format_function_expression() {
        // Expression::Function routes through format_function_decl with synthetic name
        let l = loc();
        let f = FunctionDeclaration {
            name: "<lambda>".to_string(),
            type_params: vec![],
            params: vec![Parameter {
                name: "x".to_string(),
                type_node: ty("Int"),
                default_value: None,
                is_out: false,
                is_mutable: false,
                location: l.clone(),
                doc_comment: None,
            }],
            return_types: vec![ReturnDeclaration {
                name: None,
                type_node: ty("Int"),
                doc_comment: None,
                location: l.clone(),
            }],
            body: vec![expr_stmt(ident("x"))],
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: l,
            doc_comment: None,
        };
        let prog = make_program(vec![expr_stmt(Expression::Function(f))]);
        let result = format_program(&prog);
        assert!(result.contains("def <lambda>(x: Int) -> Int"));
    }

    // ── Test block ───────────────────────────────────────────────────

    #[test]
    fn test_format_test_decl_basic() {
        let l = loc();
        let t = TestDeclaration {
            name: "Suite".to_string(),
            setup: vec![],
            before_all: None,
            before_each: None,
            cases: vec![TestCase {
                description: "passes".to_string(),
                body: vec![expr_stmt(ident("assertion"))],
                location: l.clone(),
            }],
            after_each: None,
            after_all: None,
            location: l,
        };
        let result = format_statement(&Statement::Test(t), "");
        assert!(result.starts_with("test Suite"));
        assert!(result.contains("it 'passes'"));
        assert!(result.contains("  assertion"));
        assert!(result.ends_with("end"));
    }

    #[test]
    fn test_format_test_decl_with_lifecycle() {
        let l = loc();
        let t = TestDeclaration {
            name: "Full".to_string(),
            setup: vec![expr_stmt(ident("setup"))],
            before_all: Some(vec![expr_stmt(ident("ba"))]),
            before_each: Some(vec![expr_stmt(ident("be"))]),
            cases: vec![TestCase {
                description: "case1".to_string(),
                body: vec![],
                location: l.clone(),
            }],
            after_each: Some(vec![expr_stmt(ident("ae"))]),
            after_all: Some(vec![expr_stmt(ident("aa"))]),
            location: l,
        };
        let result = format_statement(&Statement::Test(t), "");
        assert!(result.contains("beforeAll"));
        assert!(result.contains("beforeEach"));
        assert!(result.contains("afterEach"));
        assert!(result.contains("afterAll"));
        assert!(result.contains("it 'case1'"));
    }

    // ── FunctionTypeDef ─────────────────────────────────────────────

    #[test]
    fn test_format_function_type_def() {
        let l = loc();
        let ftd = FunctionTypeDefDeclaration {
            name: "OnClick".to_string(),
            type_params: vec![],
            params: vec![Parameter {
                name: "e".to_string(),
                type_node: ty("String"),
                default_value: None,
                is_out: false,
                is_mutable: false,
                location: l.clone(),
                doc_comment: Some("The event.".to_string()),
            }],
            return_types: vec![ReturnDeclaration {
                name: None,
                type_node: ty("Void"),
                doc_comment: None,
                location: l.clone(),
            }],
            is_private: false,
            doc_comment: Some("A click handler.".to_string()),
            location: l,
        };
        let result = format_statement(&Statement::FunctionTypeDef(ftd), "");
        assert!(result.starts_with("# A click handler."));
        assert!(result.contains("type def OnClick"));
        assert!(result.contains("# The event."));
        assert!(result.contains("@param e String"));
        assert!(result.contains("@return Void"));
        assert!(result.ends_with("end"));
    }

    // ── format_field with default ────────────────────────────────────

    #[test]
    fn test_format_field_with_default() {
        let l = loc();
        let f = FieldDeclaration {
            name: "count".to_string(),
            type_node: ty("Int"),
            default_value: Some(num(0.0)),
            attributes: vec![],
            location: l,
        };
        assert_eq!(format_field(&f), "count Int = 0");
    }

    // ── Non-empty array and dict (multi-line format) ─────────────────

    #[test]
    fn test_format_nonempty_array() {
        let prog = make_program(vec![expr_stmt(Expression::Array(ArrayExpr {
            items: vec![num(1.0), num(2.0), num(3.0)],
            style: ArrayLiteralStyle::Inline,
            location: loc(),
        }))]);
        let result = format_program(&prog);
        assert_eq!(result, "[1 2 3]\n");
    }

    #[test]
    fn test_format_vertical_array_preserves_multiline_shape() {
        let prog = make_program(vec![expr_stmt(Expression::Array(ArrayExpr {
            items: vec![num(1.0), num(2.0), num(3.0)],
            style: ArrayLiteralStyle::Vertical,
            location: loc(),
        }))]);
        let result = format_program(&prog);
        assert_eq!(result, "[\n  1\n  2\n  3\n]\n");
    }

    #[test]
    fn test_format_vertical_nested_array_inserts_commas_to_avoid_index_parse() {
        let prog = make_program(vec![expr_stmt(Expression::Array(ArrayExpr {
            items: vec![
                Expression::Array(ArrayExpr {
                    items: vec![num(1.0), num(2.0)],
                    style: ArrayLiteralStyle::Vertical,
                    location: loc(),
                }),
                Expression::Array(ArrayExpr {
                    items: vec![num(3.0), num(4.0)],
                    style: ArrayLiteralStyle::Vertical,
                    location: loc(),
                }),
            ],
            style: ArrayLiteralStyle::Vertical,
            location: loc(),
        }))]);
        let result = format_program(&prog);
        assert_eq!(
            result,
            "[\n  [\n    1\n    2\n  ],\n  [\n    3\n    4\n  ]\n]\n"
        );
    }

    #[test]
    fn test_format_path_preserves_inline_array_style() {
        let dir = std::env::temp_dir().join("fai_fmt_inline_array_style");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.fai");
        std::fs::write(
            &file,
            "def main\n    @return Void\ndo\n  let xs = [1 2 3]\n  print(length(xs))\nend\n",
        )
        .unwrap();
        let result = format_path(file.to_str().unwrap(), false).unwrap();
        assert_eq!(result.len(), 1);
        let formatted = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            formatted,
            "def main\n    @return Void\ndo\n  let xs = [1 2 3]\n  print(length(xs))\nend\n"
        );
    }

    #[test]
    fn test_format_path_preserves_vertical_array_style() {
        let dir = std::env::temp_dir().join("fai_fmt_vertical_array_style");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.fai");
        std::fs::write(
            &file,
            "def main\n    @return Void\ndo\n  let xs = [\n    1\n    2\n    3\n  ]\n  print(length(xs))\nend\n",
        )
        .unwrap();
        let result = format_path(file.to_str().unwrap(), false).unwrap();
        assert_eq!(result.len(), 1);
        let formatted = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            formatted,
            "def main\n    @return Void\ndo\n  let xs = [\n    1\n    2\n    3\n  ]\n  print(length(xs))\nend\n"
        );
    }

    #[test]
    fn test_format_nonempty_dict() {
        let prog = make_program(vec![expr_stmt(Expression::Dictionary(DictionaryExpr {
            entries: vec![DictionaryEntry {
                key: "name".to_string(),
                value: Expression::String(StringExpr {
                    value: "alice".to_string(),
                    location: loc(),
                }),
                location: loc(),
            }],
            location: loc(),
        }))]);
        let result = format_program(&prog);
        assert!(result.contains("{\n"));
        assert!(result.contains("name: 'alice'"));
        assert!(result.contains("\n}"));
    }

    // ── Template string ──────────────────────────────────────────────

    #[test]
    fn test_format_template_string_with_expr() {
        let prog = make_program(vec![expr_stmt(Expression::TemplateString(
            TemplateStringExpr {
                parts: vec![
                    TemplateStringPart::Text("hello ".to_string()),
                    TemplateStringPart::Expr(ident("name")),
                    TemplateStringPart::Text("!".to_string()),
                ],
                location: loc(),
            },
        ))]);
        let result = format_program(&prog);
        assert!(result.contains("\"hello "));
        assert!(result.contains("name"));
    }

    #[test]
    fn test_format_template_string_escapes_special_characters() {
        let prog = make_program(vec![expr_stmt(Expression::TemplateString(
            TemplateStringExpr {
                parts: vec![
                    TemplateStringPart::Text("line1\n".to_string()),
                    TemplateStringPart::Expr(ident("name")),
                    TemplateStringPart::Text("\t\"quoted\"\\tail".to_string()),
                ],
                location: loc(),
            },
        ))]);
        let result = format_program(&prog);
        assert!(result.contains("\"line1\\n"));
        assert!(result.contains("{{name}}"));
        assert!(result.contains("\\t\\\"quoted\\\"\\\\tail\""));
        assert!(!result.contains("\"\"\""));
    }

    #[test]
    fn test_format_plain_string_escapes_special_characters() {
        let prog = make_program(vec![expr_stmt(Expression::String(StringExpr {
            value: "line1\nline2\t'\\'".to_string(),
            location: loc(),
        }))]);
        let result = format_program(&prog);
        assert_eq!(result, "'line1\\nline2\\t\\'\\\\\\''\n");
    }

    // ── Extern block ─────────────────────────────────────────────────

    #[test]
    fn test_format_extern_block() {
        let l = loc();
        let ext = ExternBlockDeclaration {
            library: "libc".to_string(),
            types: vec![ExternTypeDecl {
                name: "FILE".to_string(),
                location: l.clone(),
            }],
            functions: vec![ExternFunctionDecl {
                name: "strlen".to_string(),
                params: vec![Parameter {
                    name: "s".to_string(),
                    type_node: ty("String"),
                    default_value: None,
                    is_out: false,
                    is_mutable: false,
                    location: l.clone(),
                    doc_comment: None,
                }],
                return_type: Some(ty("Int")),
                fixed_arg_count: None,
                location: l.clone(),
            }],
            is_private: false,
            location: l,
        };
        let result = format_statement(&Statement::ExternBlock(ext), "");
        assert!(result.contains("extern libc"));
        assert!(result.contains("type FILE"));
        assert!(result.contains("def strlen(s: String) -> Int"));
        assert!(result.ends_with("end"));
    }

    // ── Recursive directory ──────────────────────────────────────────

    #[test]
    fn test_format_path_recursive() {
        let dir = std::env::temp_dir().join("fai_fmt_recursive");
        let subdir = dir.join("sub");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(dir.join("a.fai"), "let x = 1\n").unwrap();
        std::fs::write(subdir.join("b.fai"), "let y = 2\n").unwrap();

        let results = format_path(dir.to_str().unwrap(), false).unwrap();
        assert_eq!(results.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── FunctionTypeDef without doc comments ────────────────────────

    #[test]
    fn test_format_function_type_def_no_docs() {
        // Exercises the None branch of doc_comment checks (lines 210, 217)
        let l = loc();
        let ftd = FunctionTypeDefDeclaration {
            name: "Callback".to_string(),
            type_params: vec![],
            params: vec![Parameter {
                name: "x".to_string(),
                type_node: ty("Int"),
                default_value: None,
                is_out: false,
                is_mutable: false,
                location: l.clone(),
                doc_comment: None,
            }],
            return_types: vec![],
            is_private: false,
            doc_comment: None,
            location: l,
        };
        let result = format_statement(&Statement::FunctionTypeDef(ftd), "");
        assert!(result.contains("type def Callback"));
        assert!(result.contains("@param x Int"));
    }

    // ── format_function_decl_v2 type param without doc ───────────────

    #[test]
    fn test_format_function_decl_v2_type_param_no_doc() {
        // Exercises the None branch of tp.doc_comment (line 295)
        let l = loc();
        let f = FunctionDeclaration {
            name: "id".to_string(),
            type_params: vec![TypeParamDeclaration {
                name: "T".to_string(),
                doc_comment: None,
                location: l.clone(),
            }],
            params: vec![],
            return_types: vec![],
            body: vec![],
            is_private: false,
            is_abstract: false,
            is_remote: false,
            location: l,
            doc_comment: Some("Identity.".to_string()),
        };
        let result = format_function_decl(&f, "");
        assert!(result.contains("@type T"));
        assert!(!result.contains("# The"));
    }

    // ── Tuple expression ──────────────────────────────────────────────

    #[test]
    fn test_format_tuple_expression() {
        // Exercises Expression::Tuple (lines 594-600)
        let prog = make_program(vec![expr_stmt(Expression::Tuple(TupleExpr {
            items: vec![num(1.0), num(2.0), num(3.0)],
            location: loc(),
        }))]);
        let result = format_program(&prog);
        assert!(result.contains("1, 2, 3"));
    }
}
