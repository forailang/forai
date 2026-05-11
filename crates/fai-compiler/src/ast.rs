//! AST types matching the TypeScript parser output.
//!
//! These are deserialized from JSON produced by the TS frontend.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Program {
    pub kind: String,
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
pub enum Statement {
    UseStatement(UseStatement),
    LetStatement(LetStatement),
    VarStatement(VarStatement),
    AssignmentStatement(AssignmentStatement),
    FunctionDeclaration(FunctionDeclaration),
    TypeDeclaration(TypeDeclaration),
    EnumDeclaration(EnumDeclaration),
    TestDeclaration(TestDeclaration),
    IfStatement(IfStatement),
    CaseStatement(CaseStatement),
    TryStatement(TryStatement),
    ThrowStatement(ThrowStatement),
    ForStatement(ForStatement),
    WhileStatement(WhileStatement),
    BreakStatement(BreakStatement),
    ContinueStatement(ContinueStatement),
    ReturnStatement(ReturnStatement),
    ExpressionStatement(ExpressionStatement),
    ExternBlockDeclaration(ExternBlockDeclaration),
    NowaitStatement(NowaitStatement),
    FunctionTypeDefDeclaration(FunctionTypeDefDeclaration),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
pub enum Expression {
    IdentifierExpression(IdentifierExpression),
    StringExpression(StringExpression),
    TemplateStringExpression(TemplateStringExpression),
    NumberExpression(NumberExpression),
    BooleanExpression(BooleanExpression),
    NullExpression(NullExpression),
    ArrayExpression(ArrayExpression),
    DictionaryExpression(DictionaryExpression),
    TupleExpression(TupleExpression),
    RangeExpression(RangeExpression),
    CallExpression(CallExpression),
    MemberExpression(MemberExpression),
    UnaryExpression(UnaryExpression),
    OptionalCheckExpression(OptionalCheckExpression),
    ForceUnwrapExpression(ForceUnwrapExpression),
    BinaryExpression(BinaryExpression),
    IndexExpression(IndexExpression),
    FunctionExpression(FunctionDeclaration),
}

// ── Source location ────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
pub struct SourceLocation {
    pub line: u32,
    pub column: u32,
}

// ── Statements ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UseStatement {
    pub module_path: Vec<String>,
    pub imported_names: Option<Vec<String>>,
    #[serde(default)]
    pub import_all: bool,
    #[serde(default)]
    pub is_remote: bool,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LetStatement {
    pub bindings: Vec<BindingDeclaration>,
    pub value: Expression,
    pub is_private: Option<bool>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VarStatement {
    pub bindings: Vec<BindingDeclaration>,
    pub value: Expression,
    pub is_private: Option<bool>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignmentStatement {
    pub target: AssignmentTarget,
    pub value: Expression,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
pub enum AssignmentTarget {
    #[serde(rename = "variables")]
    Variables { names: Vec<String> },
    #[serde(rename = "field")]
    Field { object: Box<Expression> },
    #[serde(rename = "index")]
    Index { object: Box<Expression> },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingDeclaration {
    pub name: String,
    pub type_name: Option<TypeNode>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionDeclaration {
    pub name: String,
    #[serde(default)]
    pub type_params: Vec<TypeParamDeclaration>,
    pub params: Vec<Parameter>,
    pub return_types: Vec<ReturnDeclaration>,
    pub body: Vec<Statement>,
    pub doc: Option<DocBlock>,
    pub is_private: Option<bool>,
    #[serde(default)]
    pub is_abstract: bool,
    #[serde(default)]
    pub is_remote: bool,
    pub location: SourceLocation,
    #[serde(default)]
    pub doc_comment: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeParamDeclaration {
    pub name: String,
    #[serde(default)]
    pub doc_comment: Option<String>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "type")]
    pub type_node: TypeNode,
    pub default_value: Option<Expression>,
    #[serde(default)]
    pub is_out: bool,
    #[serde(default)]
    pub is_mutable: bool,
    pub location: SourceLocation,
    #[serde(default)]
    pub doc_comment: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReturnDeclaration {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub type_node: TypeNode,
    #[serde(default)]
    pub doc_comment: Option<String>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionTypeDefDeclaration {
    pub name: String,
    #[serde(default)]
    pub type_params: Vec<TypeParamDeclaration>,
    pub params: Vec<Parameter>,
    pub return_types: Vec<ReturnDeclaration>,
    pub is_private: Option<bool>,
    #[serde(default)]
    pub doc_comment: Option<String>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeDeclaration {
    pub name: String,
    #[serde(default)]
    pub type_params: Vec<TypeParamDeclaration>,
    pub fields: Vec<FieldDeclaration>,
    pub doc: Option<DocBlock>,
    pub is_private: Option<bool>,
    #[serde(default)]
    pub is_remote: bool,
    pub location: SourceLocation,
}

/// Value half of a field attribute from the compiler/TS AST.
/// `string_value: None` means bare flag; `Some(s)` means string value.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldAttribute {
    pub key: String,
    #[serde(default)]
    pub string_value: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldDeclaration {
    pub name: String,
    #[serde(rename = "type")]
    pub type_node: TypeNode,
    pub default_value: Option<Expression>,
    #[serde(default)]
    pub attributes: Vec<FieldAttribute>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnumDeclaration {
    pub name: String,
    pub members: Vec<String>,
    pub doc: Option<DocBlock>,
    pub is_private: Option<bool>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternBlockDeclaration {
    pub library: String,
    pub types: Vec<ExternTypeDecl>,
    pub functions: Vec<ExternFunctionDecl>,
    pub is_private: Option<bool>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternTypeDecl {
    pub name: String,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternFunctionDecl {
    pub name: String,
    pub params: Vec<Parameter>,
    pub return_type: Option<TypeNode>,
    pub fixed_arg_count: Option<usize>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestDeclaration {
    pub name: String,
    pub setup: Vec<Statement>,
    pub before_all: Option<Vec<Statement>>,
    pub before_each: Option<Vec<Statement>>,
    pub cases: Vec<TestCase>,
    pub after_each: Option<Vec<Statement>>,
    pub after_all: Option<Vec<Statement>>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestCase {
    pub description: String,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpressionStatement {
    pub expression: Expression,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IfStatement {
    pub branches: Vec<IfBranch>,
    pub else_branch: Option<Vec<Statement>>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IfBranch {
    pub condition: Expression,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseStatement {
    pub value: Expression,
    pub when_branches: Vec<CaseBranch>,
    pub default_branch: Option<Vec<Statement>>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseBranch {
    #[serde(rename = "match")]
    pub match_expr: Expression,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TryStatement {
    pub try_body: Vec<Statement>,
    pub catch_name: String,
    pub catch_body: Vec<Statement>,
    pub finally_body: Option<Vec<Statement>>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThrowStatement {
    pub expression: Expression,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NowaitStatement {
    pub expression: Expression,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForStatement {
    pub item_name: String,
    pub items: Expression,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WhileStatement {
    pub condition: Expression,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakStatement {
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinueStatement {
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReturnStatement {
    pub value: Option<Expression>,
    pub location: SourceLocation,
}

// ── Expressions ────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentifierExpression {
    pub name: String,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StringExpression {
    pub value: String,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateStringExpression {
    pub parts: Vec<TemplateStringPart>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
pub enum TemplateStringPart {
    #[serde(rename = "text")]
    Text { value: String },
    #[serde(rename = "expression")]
    Expression { expression: Expression },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberExpression {
    pub value: f64,
    pub is_float: bool,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BooleanExpression {
    pub value: bool,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NullExpression {
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ArrayLiteralStyle {
    Inline,
    Vertical,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArrayExpression {
    pub items: Vec<Expression>,
    pub style: ArrayLiteralStyle,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryExpression {
    pub entries: Vec<DictionaryEntry>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryEntry {
    pub key: String,
    pub value: Expression,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TupleExpression {
    pub items: Vec<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeExpression {
    pub start: Box<Expression>,
    pub end: Box<Expression>,
    /// `..` is exclusive (stops at end - 1), `...` is inclusive.
    #[serde(default)]
    pub inclusive: bool,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallExpression {
    pub callee: Box<Expression>,
    pub args: Vec<CallArgument>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallArgument {
    pub label: Option<String>,
    pub value: Expression,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberExpression {
    pub object: Box<Expression>,
    pub property: String,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnaryExpression {
    pub operator: String,
    pub expression: Box<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OptionalCheckExpression {
    pub expression: Box<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForceUnwrapExpression {
    pub expression: Box<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryExpression {
    pub left: Box<Expression>,
    pub operator: String,
    pub right: Box<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexExpression {
    pub object: Box<Expression>,
    pub index: Box<Expression>,
    pub location: SourceLocation,
}

// ── Type annotations ───────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeNode {
    pub kind: String,
    pub name: Option<String>,
    pub is_type_parameter: Option<bool>,
    pub function_params: Option<Vec<TypeNode>>,
    pub function_returns: Option<Vec<TypeNode>>,
    pub is_array: bool,
    pub is_optional: bool,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DocBlock {
    pub lines: Vec<String>,
}
