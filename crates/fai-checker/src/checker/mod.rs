//! Main type checker — walks the AST and validates types.
//!
//! The checker is split into submodules by responsibility. Each submodule
//! adds methods to the shared `Checker` struct via its own `impl Checker`
//! block. Free helper functions live in `resolve.rs`.

use std::collections::HashMap;
use std::collections::HashSet;

use fai_compiler::ast::*;

use crate::builtins;
use crate::error::CheckError;
use crate::std_modules;
use crate::types::*;

mod expressions;
mod lint_rpc_secrets;
mod program;
mod resolve;
mod statements;

/// Hash-map key for `expression_types`. The first three fields identify
/// the expression's start position; the last two identify its right
/// child's start position, which disambiguates nested left-recursive
/// expressions (notably `BinaryExpression` — `a + b + c` and its inner
/// `a + b` share a leftmost column). Leaf expressions use `(0, 0)` for
/// the trailing pair.
pub type ExpressionKey = (String, u32, u32, u32, u32);

/// Compute the `ExpressionKey` for `expr` in the given module.
/// Mirrored by the codegen side (see `fai-codegen-wasm::direct`) so
/// both sides agree on the key layout.
pub fn expression_key(expr: &Expression, module_key: String) -> ExpressionKey {
    let loc = expression_location(expr);
    let (rline, rcol) = match expr {
        Expression::BinaryExpression(be) => {
            let r = expression_location(&be.right);
            (r.line, r.column)
        }
        _ => (0, 0),
    };
    (module_key, loc.line, loc.column, rline, rcol)
}

/// Depth of a `MemberExpression` within its receiver chain: the number
/// of nested `MemberExpression`s in the object spine. `a.b` is depth 0
/// (object `a` is not a member), `a.b.c` is depth 1, `a.b.c.d` depth 2.
/// Because a MemberExpression's source location is its receiver's
/// location, every level of one chain shares (line, col); this depth is
/// the stable per-level disambiguator both the checker and direct
/// codegen use to key `record_field_read_sites`.
pub fn member_chain_depth(me: &MemberExpression) -> u32 {
    let mut depth = 0;
    let mut cur = &*me.object;
    while let Expression::MemberExpression(inner) = cur {
        depth += 1;
        cur = &*inner.object;
    }
    depth
}

/// Source location of a statement — every statement node carries one.
/// Used as the fallback error location for statement-level checks so
/// diagnostics point at the offending statement, not the enclosing
/// `def` line (plan 130 A1). Declaration variants are included for
/// completeness even though `check_program` attaches their locations
/// itself.
pub(super) fn statement_location(stmt: &Statement) -> &SourceLocation {
    match stmt {
        Statement::UseStatement(s) => &s.location,
        Statement::LetStatement(s) => &s.location,
        Statement::VarStatement(s) => &s.location,
        Statement::AssignmentStatement(s) => &s.location,
        Statement::FunctionDeclaration(s) => &s.location,
        Statement::TypeDeclaration(s) => &s.location,
        Statement::EnumDeclaration(s) => &s.location,
        Statement::TestDeclaration(s) => &s.location,
        Statement::IfStatement(s) => &s.location,
        Statement::CaseStatement(s) => &s.location,
        Statement::TryStatement(s) => &s.location,
        Statement::ThrowStatement(s) => &s.location,
        Statement::ForStatement(s) => &s.location,
        Statement::WhileStatement(s) => &s.location,
        Statement::BreakStatement(s) => &s.location,
        Statement::ContinueStatement(s) => &s.location,
        Statement::ReturnStatement(s) => &s.location,
        Statement::ExpressionStatement(s) => &s.location,
        Statement::ExternBlockDeclaration(s) => &s.location,
        Statement::NowaitStatement(s) => &s.location,
        Statement::FunctionTypeDefDeclaration(s) => &s.location,
    }
}

fn expression_location(expr: &Expression) -> &SourceLocation {
    match expr {
        Expression::IdentifierExpression(e) => &e.location,
        Expression::StringExpression(e) => &e.location,
        Expression::TemplateStringExpression(e) => &e.location,
        Expression::NumberExpression(e) => &e.location,
        Expression::BooleanExpression(e) => &e.location,
        Expression::NullExpression(e) => &e.location,
        Expression::ArrayExpression(e) => &e.location,
        Expression::DictionaryExpression(e) => &e.location,
        Expression::TupleExpression(e) => &e.location,
        Expression::RangeExpression(e) => &e.location,
        Expression::CallExpression(e) => &e.location,
        Expression::MemberExpression(e) => &e.location,
        Expression::UnaryExpression(e) => &e.location,
        Expression::OptionalCheckExpression(e) => &e.location,
        Expression::ForceUnwrapExpression(e) => &e.location,
        Expression::BinaryExpression(e) => &e.location,
        Expression::IndexExpression(e) => &e.location,
        Expression::FunctionExpression(e) => &e.location,
    }
}

/// Prepared program data for type checking.
pub struct PreparedModule {
    pub name: String,
    pub statements: Vec<Statement>,
    /// Per-statement source file path, parallel to `statements`. The
    /// checker reads this before checking each top-level statement
    /// to set `Checker::current_file`, so error messages can group
    /// by file.
    pub file_paths: Vec<Option<String>>,
    pub private_names: Vec<String>,
    /// Single file fallback. Kept for callers that build small
    /// single-file modules in tests; ignored when `file_paths` is
    /// populated.
    pub file_path: Option<String>,
}

/// The main type checker.
pub struct Checker {
    pub(super) builtins: HashMap<String, Type>,
    pub(super) std_exports: HashMap<String, Vec<(String, String)>>,
    pub(super) type_declarations: HashMap<String, TypeDeclaration>,
    pub(super) enum_declarations: HashMap<String, Vec<String>>,
    pub(super) type_fields: HashMap<String, HashMap<String, Type>>,
    pub(super) extern_types: HashSet<String>,
    pub(super) loop_depth: u32,
    /// Set while checking the *forked* call of a `nowait`/`all` (the outermost
    /// call expression), consumed by the call-check to reject a forked target
    /// with `mutable` params: a detached task holding a mutable reference would
    /// outlive the caller's binding. Cleared once the outermost call is seen so
    /// nested calls in the fork's args aren't affected.
    pub(super) in_nowait_fork: bool,
    pub(super) current_file: Option<String>,
    /// Name of the module currently being checked, used to disambiguate
    /// source-location keys in `ufcs_calls` and `named_param_reorder`.
    /// `None` while checking the entry module.
    pub(super) current_module: Option<String>,
    /// Locations of call expressions rewritten by UFCS, keyed by
    /// (module_name, line, column). The module prefix matters because
    /// (line, col) alone is NOT unique across files — two unrelated
    /// calls in different modules can share coordinates and would
    /// otherwise stomp each other's metadata.
    pub ufcs_calls: HashSet<(String, u32, u32)>,
    /// Named parameter reorderings: for calls that use named params out of order,
    /// maps (module_name, line, col) -> Vec<usize> where vec[param_idx] = arg_idx.
    /// Only populated when the call-site arg order differs from the definition order.
    /// See `ufcs_calls` above for the reason the module name is part of the key.
    pub named_param_reorder: HashMap<(String, u32, u32), Vec<Option<usize>>>,
    /// Static type proven for each successfully checked expression, keyed by
    /// `ExpressionKey` — (module_name, line, column, right_line, right_column).
    /// The two trailing fields disambiguate nested left-recursive expressions
    /// (most importantly `BinaryExpression`) that share a leftmost location;
    /// they hold the right operand's source position, or `(0, 0)` for leaf
    /// expressions. This feeds direct wasm codegen decisions such as whether
    /// a value can stay in a raw primitive wasm shape.
    pub expression_types: HashMap<ExpressionKey, Type>,
    /// Warnings collected during checking (not fatal).
    pub warnings: Vec<String>,
    /// Generic type args resolved at call sites, keyed by (module_name, call_line, call_col).
    /// Value is an ordered list of concrete type constructor names for each `@type` param.
    /// An empty string means the param was not resolved to a known user type.
    /// Same key convention as `ufcs_calls`.
    pub generic_type_args: HashMap<(String, u32, u32), Vec<String>>,
    /// Index expressions `arr[i]` proven to have an `Array` receiver and
    /// an `Int` index, keyed by (module_name, line, column) of the
    /// IndexExpression. Direct wasm codegen consults this to emit an
    /// inline element read instead of the polymorphic `rt_get_index`.
    /// A dedicated set is needed because an IndexExpression and its
    /// object share a source location, so their `expression_types`
    /// entries collide — the object's `Array` type can't be recovered
    /// from that map. Same module-prefixed key convention as
    /// `ufcs_calls`.
    pub array_int_index_sites: HashSet<(String, u32, u32)>,
    /// Field reads `obj.field` proven to have a user-defined record
    /// (`Type::Named`/`Type`) receiver, mapping the MemberExpression's
    /// (module_name, line, column) to the receiver's type name. Direct
    /// wasm codegen looks the type's declared field order up from its
    /// own ordered field table to emit a direct slot read instead of
    /// the string-keyed `rt_get_field` scan. Only value-position field
    /// reads are recorded — method calls go through the call path and
    /// never reach here. The key is (module, line, col, chain_depth):
    /// a MemberExpression's source location is its *receiver's*
    /// location, so every level of a chain (`a.b.c` — both `a.b` and
    /// `a.b.c` start at `a`) shares (line, col). The chain depth (count
    /// of nested MemberExpressions in the receiver spine, via
    /// `member_chain_depth`) disambiguates the levels — including
    /// repeated-property chains like `a.b.b` that the property name
    /// alone can't separate.
    pub record_field_read_sites: HashMap<(String, u32, u32, u32), String>,
    /// Errors collected during top-level statement checking. The checker
    /// attempts to continue past a failed statement so that one typo
    /// doesn't hide unrelated errors in other functions. When non-empty,
    /// `check_program`/`check_with_modules` returns a combined error.
    pub collected_errors: Vec<CheckError>,
    /// Declared secret names from the project's `[secrets]` manifest
    /// (plan 132), installed by the CLI via [`Checker::set_declared_secrets`].
    /// `Some(names)` makes `secrets.get` with a literal name outside the
    /// set a check-time error. `None` (no manifest — loose single-file
    /// runs) leaves literal names unrestricted.
    pub(super) declared_secrets: Option<HashSet<String>>,
}

impl Checker {
    pub fn new() -> Self {
        let builtins = builtins::install_builtins();
        let std_exports = std_modules::std_module_exports();
        Self {
            builtins,
            std_exports,
            type_declarations: HashMap::new(),
            enum_declarations: HashMap::new(),
            type_fields: HashMap::new(),
            extern_types: HashSet::new(),
            loop_depth: 0,
            in_nowait_fork: false,
            current_file: None,
            current_module: None,
            ufcs_calls: HashSet::new(),
            named_param_reorder: HashMap::new(),
            expression_types: HashMap::new(),
            warnings: Vec::new(),
            generic_type_args: HashMap::new(),
            array_int_index_sites: HashSet::new(),
            record_field_read_sites: HashMap::new(),
            collected_errors: Vec::new(),
            declared_secrets: None,
        }
    }

    /// Install the project's declared secret names (plan 132). With a
    /// manifest installed, `secrets.get('NAME')` on an undeclared literal
    /// name fails at check time; dynamic names stay a runtime concern.
    pub fn set_declared_secrets(&mut self, names: HashSet<String>) {
        self.declared_secrets = Some(names);
    }
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_populates_builtins() {
        let c = Checker::new();
        assert!(c.builtins.len() > 50);
        assert!(c.builtins.contains_key("print"));
    }

    #[test]
    fn test_new_has_empty_declarations() {
        let c = Checker::new();
        assert!(c.type_declarations.is_empty());
        assert!(c.enum_declarations.is_empty());
        assert!(c.extern_types.is_empty());
        assert_eq!(c.loop_depth, 0);
    }

    #[test]
    fn test_new_has_empty_tracking() {
        let c = Checker::new();
        assert!(c.ufcs_calls.is_empty());
        assert!(c.named_param_reorder.is_empty());
        assert!(c.expression_types.is_empty());
        assert!(c.warnings.is_empty());
    }

    #[test]
    fn test_default_matches_new() {
        let c = Checker::default();
        assert!(!c.builtins.is_empty());
    }

    #[test]
    fn test_std_exports_populated() {
        let c = Checker::new();
        assert!(!c.std_exports.is_empty());
    }
}
