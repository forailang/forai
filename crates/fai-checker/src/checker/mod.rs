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
    pub private_names: Vec<String>,
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
    /// Errors collected during top-level statement checking. The checker
    /// attempts to continue past a failed statement so that one typo
    /// doesn't hide unrelated errors in other functions. When non-empty,
    /// `check_program`/`check_with_modules` returns a combined error.
    pub collected_errors: Vec<CheckError>,
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
            current_file: None,
            current_module: None,
            ufcs_calls: HashSet::new(),
            named_param_reorder: HashMap::new(),
            expression_types: HashMap::new(),
            warnings: Vec::new(),
            generic_type_args: HashMap::new(),
            collected_errors: Vec::new(),
        }
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
