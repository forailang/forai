//! Native AST types for the FAI parser.

#[derive(Debug, Clone)]
pub struct SourceLocation {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug)]
pub struct Program {
    pub statements: Vec<Statement>,
    /// Comments at the very top of the file that don't attach as a
    /// doc comment (i.e. the first statement isn't a `def`). Preserved
    /// verbatim so formatter doesn't drop them and harness-style
    /// directive blocks like `# expect: ok` survive a round-trip.
    pub leading_comments: Vec<String>,
}

#[derive(Debug)]
pub enum Statement {
    Use(UseStatement),
    Let(LetStatement),
    Var(VarStatement),
    Assignment(AssignmentStatement),
    Function(FunctionDeclaration),
    Type(TypeDeclaration),
    Enum(EnumDeclaration),
    Test(TestDeclaration),
    If(IfStatement),
    Case(CaseStatement),
    Try(TryStatement),
    Throw(ThrowStatement),
    For(ForStatement),
    While(WhileStatement),
    ExternBlock(ExternBlockDeclaration),
    Nowait(NowaitStatement),
    Break(SourceLocation),
    Continue(SourceLocation),
    Return(ReturnStatement),
    Expression(ExpressionStatement),
    FunctionTypeDef(FunctionTypeDefDeclaration),
}

#[derive(Debug)]
pub struct UseStatement {
    pub module_path: Vec<String>,
    pub imported_names: Option<Vec<String>>,
    pub is_remote: bool,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct LetStatement {
    pub bindings: Vec<BindingDeclaration>,
    pub value: Expression,
    pub is_private: bool,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct VarStatement {
    pub bindings: Vec<BindingDeclaration>,
    pub value: Expression,
    pub is_private: bool,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct AssignmentStatement {
    pub target: AssignmentTarget,
    pub value: Expression,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub enum AssignmentTarget {
    /// Simple variable(s): `x = expr` or `x, y = expr`
    Variables(Vec<String>),
    /// Field access: `x.field = expr` (chain of member accesses from a root variable)
    Field(Box<Expression>),
    /// Index access: `x[i] = expr`
    Index(Box<Expression>),
}

#[derive(Debug)]
pub struct BindingDeclaration {
    pub name: String,
    pub type_name: Option<TypeNode>,
}

#[derive(Debug)]
pub struct FunctionDeclaration {
    pub name: String,
    pub type_params: Vec<TypeParamDeclaration>,
    pub params: Vec<Parameter>,
    pub return_types: Vec<ReturnDeclaration>,
    pub body: Vec<Statement>,
    pub is_private: bool,
    /// True when the function has no `do...end` body — an interface
    /// declaration that must be implemented elsewhere.
    pub is_abstract: bool,
    /// True when declared with `remote def` — this function is an RPC
    /// endpoint exposed over the network.
    pub is_remote: bool,
    pub location: SourceLocation,
    pub doc_comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TypeParamDeclaration {
    pub name: String,
    pub doc_comment: Option<String>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct Parameter {
    pub name: String,
    pub type_node: TypeNode,
    pub default_value: Option<Expression>,
    /// True for `out` parameters in extern blocks (output pointers).
    pub is_out: bool,
    /// True for `mutable` parameters — caller's value is passed by reference, not copied.
    pub is_mutable: bool,
    pub location: SourceLocation,
    pub doc_comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReturnDeclaration {
    pub name: Option<String>,
    pub type_node: TypeNode,
    pub doc_comment: Option<String>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct FunctionTypeDefDeclaration {
    pub name: String,
    pub type_params: Vec<TypeParamDeclaration>,
    pub params: Vec<Parameter>,
    pub return_types: Vec<ReturnDeclaration>,
    pub is_private: bool,
    pub doc_comment: Option<String>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct TypeDeclaration {
    pub name: String,
    pub type_params: Vec<TypeParamDeclaration>,
    pub fields: Vec<FieldDeclaration>,
    pub is_private: bool,
    /// True when declared with `remote type` — this type is part of
    /// the RPC interface and will be serialized over the wire.
    pub is_remote: bool,
    pub location: SourceLocation,
}

/// The value half of a field attribute: either a string literal or a bare flag.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldAttributeValue {
    String(String),
    Flag,
}

/// A single key/value (or key-only flag) annotation on a type field.
///
/// Written as a comma-separated modifier after the field type:
///   `userName String, alias: "user_name", omit`
#[derive(Debug, Clone, PartialEq)]
pub struct FieldAttribute {
    pub key: String,
    pub value: FieldAttributeValue,
}

#[derive(Debug)]
pub struct FieldDeclaration {
    pub name: String,
    pub type_node: TypeNode,
    pub default_value: Option<Expression>,
    /// Arbitrary key/value metadata on this field.  The serialization system
    /// recognises `alias` (rename on the wire) and `omit` (skip entirely).
    /// Other keys are passed through for library use.
    pub attributes: Vec<FieldAttribute>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct EnumDeclaration {
    pub name: String,
    pub members: Vec<String>,
    pub is_private: bool,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct ExternBlockDeclaration {
    pub library: String,
    pub types: Vec<ExternTypeDecl>,
    pub functions: Vec<ExternFunctionDecl>,
    pub is_private: bool,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct ExternTypeDecl {
    pub name: String,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct ExternFunctionDecl {
    pub name: String,
    pub params: Vec<Parameter>,
    pub return_type: Option<TypeNode>,
    /// For variadic C functions: number of fixed args (None = not variadic).
    pub fixed_arg_count: Option<usize>,
    pub location: SourceLocation,
}

#[derive(Debug)]
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

#[derive(Debug)]
pub struct TestCase {
    pub description: String,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct ExpressionStatement {
    pub expression: Expression,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct IfStatement {
    pub branches: Vec<IfBranch>,
    pub else_branch: Option<Vec<Statement>>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct IfBranch {
    pub condition: Expression,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct CaseStatement {
    pub value: Expression,
    pub when_branches: Vec<CaseBranch>,
    pub default_branch: Option<Vec<Statement>>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct CaseBranch {
    pub match_expr: Expression,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct TryStatement {
    pub try_body: Vec<Statement>,
    pub catch_name: String,
    pub catch_body: Vec<Statement>,
    pub finally_body: Option<Vec<Statement>>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct ThrowStatement {
    pub expression: Expression,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct ReturnStatement {
    /// `None` for bare `return` (Void function); `Some` for
    /// `return <expr>`.
    pub value: Option<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct NowaitStatement {
    pub expression: Expression,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct ForStatement {
    pub item_name: String,
    pub items: Expression,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct WhileStatement {
    pub condition: Expression,
    pub body: Vec<Statement>,
    pub location: SourceLocation,
}

// ── Expressions ────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Expression {
    Identifier(IdentifierExpr),
    String(StringExpr),
    TemplateString(TemplateStringExpr),
    Number(NumberExpr),
    Boolean(BooleanExpr),
    Null(SourceLocation),
    Array(ArrayExpr),
    Dictionary(DictionaryExpr),
    Tuple(TupleExpr),
    Range(RangeExpr),
    Call(CallExpr),
    Member(MemberExpr),
    Unary(UnaryExpr),
    OptionalCheck(Box<Expression>, SourceLocation),
    ForceUnwrap(Box<Expression>, SourceLocation),
    Binary(BinaryExpr),
    Index(IndexExpr),
    /// Anonymous function: `def (params) -> ReturnType ... end`
    Function(FunctionDeclaration),
}

impl Expression {
    pub fn location(&self) -> &SourceLocation {
        match self {
            Expression::Identifier(e) => &e.location,
            Expression::String(e) => &e.location,
            Expression::TemplateString(e) => &e.location,
            Expression::Number(e) => &e.location,
            Expression::Boolean(e) => &e.location,
            Expression::Null(loc) => loc,
            Expression::Array(e) => &e.location,
            Expression::Dictionary(e) => &e.location,
            Expression::Tuple(e) => &e.location,
            Expression::Range(e) => &e.location,
            Expression::Call(e) => &e.location,
            Expression::Member(e) => &e.location,
            Expression::Unary(e) => &e.location,
            Expression::OptionalCheck(_, loc) => loc,
            Expression::ForceUnwrap(_, loc) => loc,
            Expression::Binary(e) => &e.location,
            Expression::Index(e) => &e.location,
            Expression::Function(f) => &f.location,
        }
    }
}

#[derive(Debug)]
pub struct IdentifierExpr {
    pub name: String,
    pub location: SourceLocation,
}
#[derive(Debug)]
pub struct StringExpr {
    pub value: String,
    pub location: SourceLocation,
}
#[derive(Debug)]
pub struct NumberExpr {
    pub value: f64,
    pub is_float: bool,
    pub location: SourceLocation,
}
#[derive(Debug)]
pub struct BooleanExpr {
    pub value: bool,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct TemplateStringExpr {
    pub parts: Vec<TemplateStringPart>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub enum TemplateStringPart {
    Text(String),
    Expr(Expression),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayLiteralStyle {
    Inline,
    Vertical,
}

#[derive(Debug)]
pub struct ArrayExpr {
    pub items: Vec<Expression>,
    pub style: ArrayLiteralStyle,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct DictionaryExpr {
    pub entries: Vec<DictionaryEntry>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct DictionaryEntry {
    pub key: String,
    pub value: Expression,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct TupleExpr {
    pub items: Vec<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct RangeExpr {
    pub start: Box<Expression>,
    pub end: Box<Expression>,
    /// `..` is exclusive (stops at end - 1), `...` is inclusive.
    pub inclusive: bool,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct CallExpr {
    pub callee: Box<Expression>,
    pub args: Vec<CallArgument>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct CallArgument {
    pub label: Option<String>,
    pub value: Expression,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct MemberExpr {
    pub object: Box<Expression>,
    pub property: String,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct UnaryExpr {
    pub operator: String,
    pub expression: Box<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct BinaryExpr {
    pub left: Box<Expression>,
    pub operator: String,
    pub right: Box<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug)]
pub struct IndexExpr {
    pub object: Box<Expression>,
    pub index: Box<Expression>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone)]
pub struct TypeNode {
    pub name: Option<String>,
    pub is_type_parameter: bool,
    pub function_params: Option<Vec<TypeNode>>,
    pub function_returns: Option<Vec<TypeNode>>,
    pub is_array: bool,
    pub is_optional: bool,
    pub location: SourceLocation,
}
