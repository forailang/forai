//! Type representation for the FAI type checker.

use std::collections::HashMap;

/// A checked type in the FAI type system.
#[derive(Debug, Clone)]
pub enum Type {
    // Primitives
    Int,
    Float,
    String,
    Bool,
    Dictionary,
    Error,
    Void,
    // Special
    Unknown,
    Null,
    Never,
    // Compound
    Array(Box<Type>),
    Optional(Box<Type>),
    Tuple(Vec<Type>),
    Named {
        name: std::string::String,
        category: NamedCategory,
        /// Generic type bindings resolved during construction (e.g., T=Int).
        generic_bindings: HashMap<std::string::String, Type>,
    },
    TypeParameter(std::string::String),
    Function(FunctionSig),
    EnumNamespace(std::string::String),
    ModuleNamespace {
        name: std::string::String,
        exports: HashMap<std::string::String, Type>,
    },
    TypeConstructor(std::string::String),
    /// Opaque FFI pointer type (e.g., sqlite3 Db handle).
    Ptr(std::string::String),
    /// Opaque secret handle (plan 132). Carries only a secret NAME at
    /// runtime; the host resolves plaintext at egress. The checker forbids
    /// interpolation, concatenation, comparison, and case dispatch so the
    /// value cannot leak through guest-visible channels.
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedCategory {
    Type,
    Enum,
}

#[derive(Debug, Clone)]
pub struct FunctionSig {
    pub name: std::string::String,
    /// Ordered list of `@type` parameter names declared on this function.
    pub type_params: Vec<std::string::String>,
    pub params: Vec<FunctionParam>,
    pub returns: Vec<Type>,
}

#[derive(Debug, Clone)]
pub struct FunctionParam {
    pub name: std::string::String,
    pub ty: Type,
    pub has_default: bool,
    pub is_mutable: bool,
}

// Convenience constructors

pub fn array_of(item: Type) -> Type {
    Type::Array(Box::new(item))
}

pub fn optional_of(inner: Type) -> Type {
    Type::Optional(Box::new(inner))
}

pub fn tuple_of(items: Vec<Type>) -> Type {
    Type::Tuple(items)
}

pub fn named_type(name: &str, category: NamedCategory) -> Type {
    Type::Named {
        name: name.to_string(),
        category,
        generic_bindings: HashMap::new(),
    }
}

pub fn named_type_with_bindings(
    name: &str,
    category: NamedCategory,
    bindings: HashMap<std::string::String, Type>,
) -> Type {
    Type::Named {
        name: name.to_string(),
        category,
        generic_bindings: bindings,
    }
}

pub fn type_parameter(name: &str) -> Type {
    Type::TypeParameter(name.to_string())
}

pub fn function_type(name: &str, params: Vec<FunctionParam>, returns: Vec<Type>) -> Type {
    Type::Function(FunctionSig {
        name: name.to_string(),
        type_params: Vec::new(),
        params,
        returns,
    })
}

pub fn param(name: &str, ty: Type) -> FunctionParam {
    FunctionParam {
        name: name.to_string(),
        ty,
        has_default: false,
        is_mutable: false,
    }
}

pub fn param_default(name: &str, ty: Type) -> FunctionParam {
    FunctionParam {
        name: name.to_string(),
        ty,
        has_default: true,
        is_mutable: false,
    }
}

/// Check structural type equality.
/// Check if a type contains Unknown anywhere (e.g. Unknown[], Unknown).
/// Used to detect untyped expressions like `[]` whose type should be
/// refined by later context.
pub fn contains_unknown(ty: &Type) -> bool {
    match ty {
        Type::Unknown => true,
        Type::Array(inner) => contains_unknown(inner),
        Type::Optional(inner) => contains_unknown(inner),
        Type::Tuple(items) => items.iter().any(contains_unknown),
        _ => false,
    }
}

pub fn same_type(left: &Type, right: &Type) -> bool {
    match (left, right) {
        (Type::Int, Type::Int) => true,
        (Type::Float, Type::Float) => true,
        (Type::String, Type::String) => true,
        (Type::Bool, Type::Bool) => true,
        (Type::Dictionary, Type::Dictionary) => true,
        (Type::Error, Type::Error) => true,
        (Type::Void, Type::Void) => true,
        (Type::Unknown, Type::Unknown) => true,
        (Type::Null, Type::Null) => true,
        (Type::Never, Type::Never) => true,
        (Type::Array(a), Type::Array(b)) => same_type(a, b),
        (Type::Optional(a), Type::Optional(b)) => same_type(a, b),
        (Type::Tuple(a), Type::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| same_type(x, y))
        }
        (
            Type::Named {
                name: na,
                category: ca,
                ..
            },
            Type::Named {
                name: nb,
                category: cb,
                ..
            },
        ) => na == nb && ca == cb,
        (Type::TypeParameter(a), Type::TypeParameter(b)) => a == b,
        (Type::Function(a), Type::Function(b)) => {
            a.params.len() == b.params.len()
                && a.params
                    .iter()
                    .zip(b.params.iter())
                    .all(|(x, y)| same_type(&x.ty, &y.ty))
                && (
                    // Exact return type match
                    (a.returns.len() == b.returns.len()
                        && a.returns.iter().zip(b.returns.iter()).all(|(x, y)| same_type(x, y)))
                    // Void return accepts any return type (void callbacks)
                    || (b.returns.len() == 1 && matches!(b.returns[0], Type::Void))
                    || (a.returns.len() == 1 && matches!(a.returns[0], Type::Void))
                )
        }
        (Type::EnumNamespace(a), Type::EnumNamespace(b)) => a == b,
        (Type::ModuleNamespace { name: a, .. }, Type::ModuleNamespace { name: b, .. }) => a == b,
        (Type::TypeConstructor(a), Type::TypeConstructor(b)) => a == b,
        (Type::Ptr(a), Type::Ptr(b)) => a == b,
        (Type::Secret, Type::Secret) => true,
        _ => false,
    }
}

/// Numeric coercion allowed at a typed binding site without a cast.
/// Int → Float is always safe (ints are a subgroup of floats). The
/// reverse direction (narrowing Float → Int) is NOT permitted here;
/// it requires either a whole-valued Float literal (handled by
/// `refine_literal_type`) or an explicit `toInt` call.
pub fn is_numeric_coercible(from: &Type, to: &Type) -> bool {
    matches!((from, to), (Type::Int, Type::Float))
}

/// Check if `actual` is assignable to a slot expecting `expected`.
pub fn is_assignable(actual: &Type, expected: &Type) -> bool {
    if same_type(actual, expected) {
        return true;
    }
    if matches!(actual, Type::Never) {
        return true;
    }
    if matches!(actual, Type::Null) && matches!(expected, Type::Optional(_) | Type::Ptr(_)) {
        return true;
    }
    if let Type::Optional(inner) = expected {
        if same_type(actual, inner) {
            return true;
        }
    }
    if matches!(actual, Type::Unknown) || matches!(expected, Type::Unknown) {
        return true;
    }
    // TypeParameter is treated as compatible with anything (erased generics).
    // Full generic type tracking (monomorphization) would be a future enhancement.
    if matches!(actual, Type::TypeParameter(_)) || matches!(expected, Type::TypeParameter(_)) {
        return true;
    }
    // Recurse into array element types so that e.g. Unknown[] is assignable to T[].
    if let (Type::Array(a_inner), Type::Array(e_inner)) = (actual, expected) {
        return is_assignable(a_inner, e_inner);
    }
    // Function type: if expected returns Void, accept any actual return type.
    // This allows `do Label("hi") end` to satisfy a `type def Children @return Void end` param.
    if let (Type::Function(actual_sig), Type::Function(expected_sig)) = (actual, expected) {
        let params_match = actual_sig.params.len() == expected_sig.params.len()
            && actual_sig
                .params
                .iter()
                .zip(expected_sig.params.iter())
                .all(|(a, e)| is_assignable(&a.ty, &e.ty));
        let expected_void =
            expected_sig.returns.len() == 1 && matches!(expected_sig.returns[0], Type::Void);
        if params_match && expected_void {
            return true;
        }
    }
    false
}

/// Unify two branch types, allowing optional/null widening.
/// Returns the unified type, or None if incompatible.
pub fn unify_branch_type(a: &Type, b: &Type) -> Option<Type> {
    if same_type(a, b) {
        return Some(a.clone());
    }
    // null + T? → T?
    if matches!(a, Type::Null) && matches!(b, Type::Optional(_)) {
        return Some(b.clone());
    }
    if matches!(b, Type::Null) && matches!(a, Type::Optional(_)) {
        return Some(a.clone());
    }
    // T + null → T?
    if matches!(b, Type::Null) {
        return Some(Type::Optional(Box::new(a.clone())));
    }
    if matches!(a, Type::Null) {
        return Some(Type::Optional(Box::new(b.clone())));
    }
    // T + T? → T?
    if let Type::Optional(inner) = b {
        if same_type(a, inner) {
            return Some(b.clone());
        }
    }
    if let Type::Optional(inner) = a {
        if same_type(b, inner) {
            return Some(a.clone());
        }
    }
    // Void is compatible with anything (expression statements)
    if matches!(a, Type::Void) {
        return Some(b.clone());
    }
    if matches!(b, Type::Void) {
        return Some(a.clone());
    }
    None
}

/// Human-readable type description.
pub fn describe_type(ty: &Type) -> std::string::String {
    match ty {
        Type::Int => "Int".to_string(),
        Type::Float => "Float".to_string(),
        Type::String => "String".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::Dictionary => "Dictionary".to_string(),
        Type::Error => "Error".to_string(),
        Type::Void => "Void".to_string(),
        Type::Unknown => "Unknown".to_string(),
        Type::Null => "null".to_string(),
        Type::Never => "Never".to_string(),
        Type::Array(item) => format!("{}[]", describe_type(item)),
        Type::Optional(inner) => format!("{}?", describe_type(inner)),
        Type::Tuple(items) => {
            let parts: Vec<_> = items.iter().map(describe_type).collect();
            format!("({})", parts.join(", "))
        }
        Type::Named { name, .. } => name.clone(),
        Type::TypeParameter(name) => format!("${}", name),
        Type::Function(sig) => {
            let params: Vec<_> = sig.params.iter().map(|p| describe_type(&p.ty)).collect();
            let returns: Vec<_> = sig.returns.iter().map(describe_type).collect();
            format!("({}) -> {}", params.join(", "), returns.join(", "))
        }
        Type::EnumNamespace(name) => format!("{} namespace", name),
        Type::ModuleNamespace { name, .. } => format!("{} module", name),
        Type::TypeConstructor(name) => format!("{} constructor", name),
        Type::Ptr(name) => name.clone(),
        Type::Secret => "Secret".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_arr() -> Type {
        array_of(Type::Int)
    }
    fn str_opt() -> Type {
        optional_of(Type::String)
    }

    // ── same_type ────────────────────────────────────────────────────

    #[test]
    fn test_same_type_null() {
        assert!(same_type(&Type::Null, &Type::Null));
    }
    #[test]
    fn test_same_type_never() {
        assert!(same_type(&Type::Never, &Type::Never));
    }
    #[test]
    fn test_same_type_tuple() {
        let a = tuple_of(vec![Type::Int, Type::String]);
        let b = tuple_of(vec![Type::Int, Type::String]);
        assert!(same_type(&a, &b));
    }
    #[test]
    fn test_same_type_tuple_mismatch() {
        let a = tuple_of(vec![Type::Int, Type::String]);
        let b = tuple_of(vec![Type::Int, Type::Int]);
        assert!(!same_type(&a, &b));
    }
    #[test]
    fn test_same_type_function_void_return() {
        let f1 = function_type("f", vec![], vec![Type::Void]);
        let f2 = function_type("g", vec![], vec![Type::Void]);
        assert!(same_type(&f1, &f2));
    }
    #[test]
    fn test_same_type_function_any_return_matches_void_expected() {
        // When expected returns Void, any actual return type is ok
        let actual = function_type("f", vec![], vec![Type::Int]);
        let expected = function_type("g", vec![], vec![Type::Void]);
        assert!(same_type(&actual, &expected));
    }
    #[test]
    fn test_same_type_enum_namespace() {
        let a = Type::EnumNamespace("Color".to_string());
        let b = Type::EnumNamespace("Color".to_string());
        assert!(same_type(&a, &b));
        assert!(!same_type(&a, &Type::EnumNamespace("Shape".to_string())));
    }
    #[test]
    fn test_same_type_ptr() {
        let a = Type::Ptr("FILE".to_string());
        let b = Type::Ptr("FILE".to_string());
        assert!(same_type(&a, &b));
    }
    #[test]
    fn test_same_type_type_constructor() {
        let a = Type::TypeConstructor("Point".to_string());
        let b = Type::TypeConstructor("Point".to_string());
        assert!(same_type(&a, &b));
    }
    #[test]
    fn test_same_type_module_namespace() {
        let a = Type::ModuleNamespace {
            name: "math".to_string(),
            exports: HashMap::new(),
        };
        let b = Type::ModuleNamespace {
            name: "math".to_string(),
            exports: HashMap::new(),
        };
        assert!(same_type(&a, &b));
    }

    // ── is_assignable ────────────────────────────────────────────────

    #[test]
    fn test_is_assignable_never_to_anything() {
        // Never is assignable to any type
        assert!(is_assignable(&Type::Never, &Type::Int));
        assert!(is_assignable(&Type::Never, &Type::String));
        assert!(is_assignable(&Type::Never, &str_opt()));
    }
    #[test]
    fn test_is_assignable_null_to_optional() {
        assert!(is_assignable(&Type::Null, &str_opt()));
    }
    #[test]
    fn test_is_assignable_null_to_ptr() {
        assert!(is_assignable(&Type::Null, &Type::Ptr("FILE".to_string())));
    }
    #[test]
    fn test_is_assignable_value_to_matching_optional_inner() {
        // String is assignable to String?
        assert!(is_assignable(&Type::String, &str_opt()));
    }
    #[test]
    fn test_is_assignable_type_parameter_always_ok() {
        assert!(is_assignable(
            &Type::TypeParameter("T".to_string()),
            &Type::Int
        ));
        assert!(is_assignable(
            &Type::Int,
            &Type::TypeParameter("T".to_string())
        ));
    }
    #[test]
    fn test_is_assignable_function_void_return() {
        // Function returning Int is assignable to slot expecting () -> Void
        let actual = function_type("f", vec![], vec![Type::Int]);
        let expected = function_type("g", vec![], vec![Type::Void]);
        assert!(is_assignable(&actual, &expected));
    }
    #[test]
    fn test_is_assignable_unknown_wildcard() {
        assert!(is_assignable(&Type::Unknown, &Type::Int));
        assert!(is_assignable(&Type::Int, &Type::Unknown));
    }

    // ── unify_branch_type ────────────────────────────────────────────

    #[test]
    fn test_unify_null_with_optional() {
        let result = unify_branch_type(&Type::Null, &str_opt());
        assert!(matches!(result, Some(Type::Optional(_))));
    }
    #[test]
    fn test_unify_optional_with_null() {
        let result = unify_branch_type(&str_opt(), &Type::Null);
        assert!(matches!(result, Some(Type::Optional(_))));
    }
    #[test]
    fn test_unify_value_with_null_widens_to_optional() {
        let result = unify_branch_type(&Type::String, &Type::Null);
        assert!(matches!(result, Some(Type::Optional(_))));
    }
    #[test]
    fn test_unify_null_with_value_widens_to_optional() {
        let result = unify_branch_type(&Type::Null, &Type::String);
        assert!(matches!(result, Some(Type::Optional(_))));
    }
    #[test]
    fn test_unify_value_with_optional_of_same() {
        // T + T? → T?
        let result = unify_branch_type(&Type::String, &str_opt());
        assert!(matches!(result, Some(Type::Optional(_))));
    }
    #[test]
    fn test_unify_optional_with_value_of_same() {
        // T? + T → T?
        let result = unify_branch_type(&str_opt(), &Type::String);
        assert!(matches!(result, Some(Type::Optional(_))));
    }
    #[test]
    fn test_unify_void_widens_to_other() {
        // Void + T → T
        let result = unify_branch_type(&Type::Void, &Type::Int);
        assert!(matches!(result, Some(Type::Int)));
    }
    #[test]
    fn test_unify_other_with_void() {
        // T + Void → T
        let result = unify_branch_type(&Type::Int, &Type::Void);
        assert!(matches!(result, Some(Type::Int)));
    }
    #[test]
    fn test_unify_incompatible_returns_none() {
        assert!(unify_branch_type(&Type::Int, &Type::String).is_none());
    }

    // ── describe_type ────────────────────────────────────────────────

    #[test]
    fn test_describe_type_array() {
        assert_eq!(describe_type(&int_arr()), "Int[]");
    }
    #[test]
    fn test_describe_type_optional() {
        assert_eq!(describe_type(&str_opt()), "String?");
    }
    #[test]
    fn test_describe_type_tuple() {
        let t = tuple_of(vec![Type::Int, Type::String]);
        assert_eq!(describe_type(&t), "(Int, String)");
    }
    #[test]
    fn test_describe_type_named() {
        let n = named_type("Point", NamedCategory::Type);
        assert_eq!(describe_type(&n), "Point");
    }
    #[test]
    fn test_describe_type_type_parameter() {
        assert_eq!(describe_type(&Type::TypeParameter("T".to_string())), "$T");
    }
    #[test]
    fn test_describe_type_function() {
        let f = function_type("f", vec![], vec![Type::Int]);
        assert!(describe_type(&f).contains("Int"));
    }
    #[test]
    fn test_describe_type_enum_namespace() {
        assert_eq!(
            describe_type(&Type::EnumNamespace("Color".to_string())),
            "Color namespace"
        );
    }
    #[test]
    fn test_describe_type_module_namespace() {
        let m = Type::ModuleNamespace {
            name: "math".to_string(),
            exports: HashMap::new(),
        };
        assert_eq!(describe_type(&m), "math module");
    }
    #[test]
    fn test_describe_type_type_constructor() {
        assert_eq!(
            describe_type(&Type::TypeConstructor("Box".to_string())),
            "Box constructor"
        );
    }
    #[test]
    fn test_describe_type_ptr() {
        assert_eq!(describe_type(&Type::Ptr("FILE".to_string())), "FILE");
    }
    #[test]
    fn test_describe_type_never() {
        assert_eq!(describe_type(&Type::Never), "Never");
    }
    #[test]
    fn test_describe_type_null() {
        assert_eq!(describe_type(&Type::Null), "null");
    }
}
