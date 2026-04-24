//! Bridge from native parser AST to the compiler's serde AST types.
//! Converts fai_parser::ast types to fai_compiler::ast types.

use crate::ast as s;
use fai_parser::ast as n;

pub fn convert_program(p: &n::Program) -> s::Program {
    s::Program {
        kind: "Program".into(),
        statements: p.statements.iter().map(convert_statement).collect(),
    }
}

fn convert_statement(stmt: &n::Statement) -> s::Statement {
    match stmt {
        n::Statement::Use(u) => s::Statement::UseStatement(s::UseStatement {
            module_path: u.module_path.clone(),
            imported_names: u.imported_names.clone(),
            is_remote: u.is_remote,
            location: loc(&u.location),
        }),
        n::Statement::Let(l) => s::Statement::LetStatement(s::LetStatement {
            bindings: l.bindings.iter().map(convert_binding).collect(),
            value: convert_expr(&l.value),
            is_private: Some(l.is_private),
            location: loc(&l.location),
        }),
        n::Statement::Var(v) => s::Statement::VarStatement(s::VarStatement {
            bindings: v.bindings.iter().map(convert_binding).collect(),
            value: convert_expr(&v.value),
            is_private: Some(v.is_private),
            location: loc(&v.location),
        }),
        n::Statement::Assignment(a) => s::Statement::AssignmentStatement(s::AssignmentStatement {
            target: match &a.target {
                n::AssignmentTarget::Variables(names) => s::AssignmentTarget::Variables {
                    names: names.clone(),
                },
                n::AssignmentTarget::Field(expr) => s::AssignmentTarget::Field {
                    object: Box::new(convert_expr(expr)),
                },
                n::AssignmentTarget::Index(expr) => s::AssignmentTarget::Index {
                    object: Box::new(convert_expr(expr)),
                },
            },
            value: convert_expr(&a.value),
            location: loc(&a.location),
        }),
        n::Statement::Function(f) => s::Statement::FunctionDeclaration(convert_fn_decl(f)),
        n::Statement::Type(t) => s::Statement::TypeDeclaration(s::TypeDeclaration {
            name: t.name.clone(),
            type_params: t
                .type_params
                .iter()
                .map(|tp| s::TypeParamDeclaration {
                    name: tp.name.clone(),
                    doc_comment: tp.doc_comment.clone(),
                    location: loc(&tp.location),
                })
                .collect(),
            fields: t
                .fields
                .iter()
                .map(|f| s::FieldDeclaration {
                    name: f.name.clone(),
                    type_node: convert_type_node(&f.type_node),
                    default_value: f.default_value.as_ref().map(convert_expr),
                    attributes: f
                        .attributes
                        .iter()
                        .map(|a| s::FieldAttribute {
                            key: a.key.clone(),
                            string_value: match &a.value {
                                n::FieldAttributeValue::String(s) => Some(s.clone()),
                                n::FieldAttributeValue::Flag => None,
                            },
                        })
                        .collect(),
                    location: loc(&f.location),
                })
                .collect(),
            doc: None,
            is_private: Some(t.is_private),
            is_remote: t.is_remote,
            location: loc(&t.location),
        }),
        n::Statement::Enum(e) => s::Statement::EnumDeclaration(s::EnumDeclaration {
            name: e.name.clone(),
            members: e.members.clone(),
            doc: None,
            is_private: Some(e.is_private),
            location: loc(&e.location),
        }),
        n::Statement::Test(t) => s::Statement::TestDeclaration(s::TestDeclaration {
            name: t.name.clone(),
            setup: t.setup.iter().map(convert_statement).collect(),
            before_all: t
                .before_all
                .as_ref()
                .map(|v| v.iter().map(convert_statement).collect()),
            before_each: t
                .before_each
                .as_ref()
                .map(|v| v.iter().map(convert_statement).collect()),
            cases: t
                .cases
                .iter()
                .map(|c| s::TestCase {
                    description: c.description.clone(),
                    body: c.body.iter().map(convert_statement).collect(),
                    location: loc(&c.location),
                })
                .collect(),
            after_each: t
                .after_each
                .as_ref()
                .map(|v| v.iter().map(convert_statement).collect()),
            after_all: t
                .after_all
                .as_ref()
                .map(|v| v.iter().map(convert_statement).collect()),
            location: loc(&t.location),
        }),
        n::Statement::If(i) => s::Statement::IfStatement(s::IfStatement {
            branches: i
                .branches
                .iter()
                .map(|b| s::IfBranch {
                    condition: convert_expr(&b.condition),
                    body: b.body.iter().map(convert_statement).collect(),
                    location: loc(&b.location),
                })
                .collect(),
            else_branch: i
                .else_branch
                .as_ref()
                .map(|v| v.iter().map(convert_statement).collect()),
            location: loc(&i.location),
        }),
        n::Statement::Case(c) => s::Statement::CaseStatement(s::CaseStatement {
            value: convert_expr(&c.value),
            when_branches: c
                .when_branches
                .iter()
                .map(|b| s::CaseBranch {
                    match_expr: convert_expr(&b.match_expr),
                    body: b.body.iter().map(convert_statement).collect(),
                    location: loc(&b.location),
                })
                .collect(),
            default_branch: c
                .default_branch
                .as_ref()
                .map(|v| v.iter().map(convert_statement).collect()),
            location: loc(&c.location),
        }),
        n::Statement::Try(t) => s::Statement::TryStatement(s::TryStatement {
            try_body: t.try_body.iter().map(convert_statement).collect(),
            catch_name: t.catch_name.clone(),
            catch_body: t.catch_body.iter().map(convert_statement).collect(),
            finally_body: t
                .finally_body
                .as_ref()
                .map(|v| v.iter().map(convert_statement).collect()),
            location: loc(&t.location),
        }),
        n::Statement::Throw(t) => s::Statement::ThrowStatement(s::ThrowStatement {
            expression: convert_expr(&t.expression),
            location: loc(&t.location),
        }),
        n::Statement::Nowait(nw) => s::Statement::NowaitStatement(s::NowaitStatement {
            expression: convert_expr(&nw.expression),
            location: loc(&nw.location),
        }),
        n::Statement::For(f) => s::Statement::ForStatement(s::ForStatement {
            item_name: f.item_name.clone(),
            items: convert_expr(&f.items),
            body: f.body.iter().map(convert_statement).collect(),
            location: loc(&f.location),
        }),
        n::Statement::While(w) => s::Statement::WhileStatement(s::WhileStatement {
            condition: convert_expr(&w.condition),
            body: w.body.iter().map(convert_statement).collect(),
            location: loc(&w.location),
        }),
        n::Statement::Break(l) => {
            s::Statement::BreakStatement(s::BreakStatement { location: loc(l) })
        }
        n::Statement::Continue(l) => {
            s::Statement::ContinueStatement(s::ContinueStatement { location: loc(l) })
        }
        n::Statement::Return(r) => s::Statement::ReturnStatement(s::ReturnStatement {
            value: r.value.as_ref().map(convert_expr),
            location: loc(&r.location),
        }),
        n::Statement::Expression(e) => s::Statement::ExpressionStatement(s::ExpressionStatement {
            expression: convert_expr(&e.expression),
            location: loc(&e.location),
        }),
        n::Statement::ExternBlock(ext) => {
            s::Statement::ExternBlockDeclaration(s::ExternBlockDeclaration {
                library: ext.library.clone(),
                types: ext
                    .types
                    .iter()
                    .map(|t| s::ExternTypeDecl {
                        name: t.name.clone(),
                        location: loc(&t.location),
                    })
                    .collect(),
                functions: ext
                    .functions
                    .iter()
                    .map(|f| s::ExternFunctionDecl {
                        name: f.name.clone(),
                        params: f
                            .params
                            .iter()
                            .map(|p| s::Parameter {
                                name: p.name.clone(),
                                type_node: convert_type_node(&p.type_node),
                                default_value: p.default_value.as_ref().map(convert_expr),
                                is_out: p.is_out,
                                is_mutable: false,
                                location: loc(&p.location),
                                doc_comment: None,
                            })
                            .collect(),
                        return_type: f.return_type.as_ref().map(convert_type_node),
                        fixed_arg_count: f.fixed_arg_count,
                        location: loc(&f.location),
                    })
                    .collect(),
                is_private: Some(ext.is_private),
                location: loc(&ext.location),
            })
        }
        n::Statement::FunctionTypeDef(ftd) => {
            s::Statement::FunctionTypeDefDeclaration(s::FunctionTypeDefDeclaration {
                name: ftd.name.clone(),
                type_params: ftd
                    .type_params
                    .iter()
                    .map(|tp| s::TypeParamDeclaration {
                        name: tp.name.clone(),
                        doc_comment: tp.doc_comment.clone(),
                        location: loc(&tp.location),
                    })
                    .collect(),
                params: ftd
                    .params
                    .iter()
                    .map(|p| s::Parameter {
                        name: p.name.clone(),
                        type_node: convert_type_node(&p.type_node),
                        default_value: None,
                        is_out: false,
                        is_mutable: false,
                        location: loc(&p.location),
                        doc_comment: p.doc_comment.clone(),
                    })
                    .collect(),
                return_types: ftd
                    .return_types
                    .iter()
                    .map(|r| s::ReturnDeclaration {
                        name: r.name.clone(),
                        type_node: convert_type_node(&r.type_node),
                        doc_comment: r.doc_comment.clone(),
                        location: loc(&r.location),
                    })
                    .collect(),
                is_private: Some(ftd.is_private),
                doc_comment: ftd.doc_comment.clone(),
                location: loc(&ftd.location),
            })
        }
    }
}

fn convert_fn_decl(f: &n::FunctionDeclaration) -> s::FunctionDeclaration {
    s::FunctionDeclaration {
        name: f.name.clone(),
        type_params: f
            .type_params
            .iter()
            .map(|tp| s::TypeParamDeclaration {
                name: tp.name.clone(),
                doc_comment: tp.doc_comment.clone(),
                location: loc(&tp.location),
            })
            .collect(),
        params: f
            .params
            .iter()
            .map(|p| s::Parameter {
                name: p.name.clone(),
                type_node: convert_type_node(&p.type_node),
                default_value: p.default_value.as_ref().map(convert_expr),
                is_out: p.is_out,
                is_mutable: p.is_mutable,
                location: loc(&p.location),
                doc_comment: p.doc_comment.clone(),
            })
            .collect(),
        return_types: f
            .return_types
            .iter()
            .map(|r| s::ReturnDeclaration {
                name: r.name.clone(),
                type_node: convert_type_node(&r.type_node),
                doc_comment: r.doc_comment.clone(),
                location: loc(&r.location),
            })
            .collect(),
        body: f.body.iter().map(convert_statement).collect(),
        doc: None,
        is_private: Some(f.is_private),
        is_abstract: f.is_abstract,
        is_remote: f.is_remote,
        location: loc(&f.location),
        doc_comment: f.doc_comment.clone(),
    }
}

fn convert_binding(b: &n::BindingDeclaration) -> s::BindingDeclaration {
    s::BindingDeclaration {
        name: b.name.clone(),
        type_name: b.type_name.as_ref().map(convert_type_node),
    }
}

fn convert_expr(e: &n::Expression) -> s::Expression {
    match e {
        n::Expression::Identifier(i) => {
            s::Expression::IdentifierExpression(s::IdentifierExpression {
                name: i.name.clone(),
                location: loc(&i.location),
            })
        }
        n::Expression::String(s_) => s::Expression::StringExpression(s::StringExpression {
            value: s_.value.clone(),
            location: loc(&s_.location),
        }),
        n::Expression::TemplateString(t) => {
            s::Expression::TemplateStringExpression(s::TemplateStringExpression {
                parts: t
                    .parts
                    .iter()
                    .map(|p| match p {
                        n::TemplateStringPart::Text(v) => {
                            s::TemplateStringPart::Text { value: v.clone() }
                        }
                        n::TemplateStringPart::Expr(e) => s::TemplateStringPart::Expression {
                            expression: convert_expr(e),
                        },
                    })
                    .collect(),
                location: loc(&t.location),
            })
        }
        n::Expression::Number(n_) => s::Expression::NumberExpression(s::NumberExpression {
            value: n_.value,
            is_float: n_.is_float,
            location: loc(&n_.location),
        }),
        n::Expression::Boolean(b) => s::Expression::BooleanExpression(s::BooleanExpression {
            value: b.value,
            location: loc(&b.location),
        }),
        n::Expression::Null(l) => {
            s::Expression::NullExpression(s::NullExpression { location: loc(l) })
        }
        n::Expression::Array(a) => s::Expression::ArrayExpression(s::ArrayExpression {
            items: a.items.iter().map(convert_expr).collect(),
            style: match a.style {
                n::ArrayLiteralStyle::Inline => s::ArrayLiteralStyle::Inline,
                n::ArrayLiteralStyle::Vertical => s::ArrayLiteralStyle::Vertical,
            },
            location: loc(&a.location),
        }),
        n::Expression::Dictionary(d) => {
            s::Expression::DictionaryExpression(s::DictionaryExpression {
                entries: d
                    .entries
                    .iter()
                    .map(|e| s::DictionaryEntry {
                        key: e.key.clone(),
                        value: convert_expr(&e.value),
                        location: loc(&e.location),
                    })
                    .collect(),
                location: loc(&d.location),
            })
        }
        n::Expression::Tuple(t) => s::Expression::TupleExpression(s::TupleExpression {
            items: t.items.iter().map(convert_expr).collect(),
            location: loc(&t.location),
        }),
        n::Expression::Range(r) => s::Expression::RangeExpression(s::RangeExpression {
            start: Box::new(convert_expr(&r.start)),
            end: Box::new(convert_expr(&r.end)),
            inclusive: r.inclusive,
            location: loc(&r.location),
        }),
        n::Expression::Call(c) => s::Expression::CallExpression(s::CallExpression {
            callee: Box::new(convert_expr(&c.callee)),
            args: c
                .args
                .iter()
                .map(|a| s::CallArgument {
                    label: a.label.clone(),
                    value: convert_expr(&a.value),
                    location: loc(&a.location),
                })
                .collect(),
            location: loc(&c.location),
        }),
        n::Expression::Member(m) => s::Expression::MemberExpression(s::MemberExpression {
            object: Box::new(convert_expr(&m.object)),
            property: m.property.clone(),
            location: loc(&m.location),
        }),
        n::Expression::Unary(u) => s::Expression::UnaryExpression(s::UnaryExpression {
            operator: u.operator.clone(),
            expression: Box::new(convert_expr(&u.expression)),
            location: loc(&u.location),
        }),
        n::Expression::OptionalCheck(e, l) => {
            s::Expression::OptionalCheckExpression(s::OptionalCheckExpression {
                expression: Box::new(convert_expr(e)),
                location: loc(l),
            })
        }
        n::Expression::ForceUnwrap(e, l) => {
            s::Expression::ForceUnwrapExpression(s::ForceUnwrapExpression {
                expression: Box::new(convert_expr(e)),
                location: loc(l),
            })
        }
        n::Expression::Binary(b) => s::Expression::BinaryExpression(s::BinaryExpression {
            left: Box::new(convert_expr(&b.left)),
            operator: b.operator.clone(),
            right: Box::new(convert_expr(&b.right)),
            location: loc(&b.location),
        }),
        n::Expression::Index(ix) => s::Expression::IndexExpression(s::IndexExpression {
            object: Box::new(convert_expr(&ix.object)),
            index: Box::new(convert_expr(&ix.index)),
            location: loc(&ix.location),
        }),
        n::Expression::Function(f) => s::Expression::FunctionExpression(convert_fn_decl(f)),
    }
}

fn convert_type_node(t: &n::TypeNode) -> s::TypeNode {
    s::TypeNode {
        kind: "TypeNode".into(),
        name: t.name.clone(),
        is_type_parameter: Some(t.is_type_parameter),
        function_params: t
            .function_params
            .as_ref()
            .map(|v| v.iter().map(convert_type_node).collect()),
        function_returns: t
            .function_returns
            .as_ref()
            .map(|v| v.iter().map(convert_type_node).collect()),
        is_array: t.is_array,
        is_optional: t.is_optional,
        location: loc(&t.location),
    }
}

fn loc(l: &n::SourceLocation) -> s::SourceLocation {
    s::SourceLocation {
        line: l.line,
        column: l.column,
    }
}
