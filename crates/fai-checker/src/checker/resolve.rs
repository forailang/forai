//! Type resolution helpers + generic binding utilities.

use std::collections::HashMap;

use fai_compiler::ast::*;

use super::Checker;
use crate::error::CheckError;
use crate::types::*;

impl Checker {
    pub(super) fn resolve_type_node(&self, node: &TypeNode) -> Result<Type, CheckError> {
        let mut base = if node.function_params.is_some() && node.function_returns.is_some() {
            let params: Vec<FunctionParam> = node
                .function_params
                .as_ref()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let ty = self.resolve_type_node(p)?;
                    Ok(FunctionParam {
                        name: format!("arg{}", i + 1),
                        ty,
                        has_default: false,
                        is_mutable: false,
                    })
                })
                .collect::<Result<_, CheckError>>()?;
            let returns: Vec<Type> = node
                .function_returns
                .as_ref()
                .unwrap()
                .iter()
                .map(|r| self.resolve_type_node(r))
                .collect::<Result<_, _>>()?;
            Type::Function(FunctionSig {
                name: "callback".to_string(),
                type_params: Vec::new(),
                params,
                returns,
            })
        } else {
            self.resolve_base_type(node.name.as_deref(), node.is_type_parameter == Some(true))?
        };

        if node.is_array {
            base = array_of(base);
        }
        if node.is_optional {
            base = optional_of(base);
        }
        Ok(base)
    }

    fn resolve_base_type(
        &self,
        name: Option<&str>,
        is_type_param: bool,
    ) -> Result<Type, CheckError> {
        let name = name.ok_or_else(|| CheckError::new("Type name is required"))?;
        if is_type_param {
            return Ok(type_parameter(name));
        }
        match name {
            "Int" => Ok(Type::Int),
            "Float" => Ok(Type::Float),
            "String" => Ok(Type::String),
            "Bool" => Ok(Type::Bool),
            "Dictionary" => Ok(Type::Dictionary),
            "Unknown" => Ok(Type::Unknown),
            "Error" => Ok(Type::Error),
            "Void" => Ok(Type::Void),
            "Ptr" => Ok(Type::Ptr("Ptr".to_string())),
            // Built-in named types (not user-defined, not extern)
            "HttpRequest"
            | "HttpResponse"
            | "Router"
            | "Event"
            | "Subscription"
            | "Cookie"
            | "RequestResponse"
            | "ServerStarted"
            | "HttpError"
            | "RpcCall"
            | "RpcResult"
            | "RpcError" => Ok(named_type(name, NamedCategory::Type)),
            _ => {
                if self.type_declarations.contains_key(name) {
                    return Ok(named_type(name, NamedCategory::Type));
                }
                if self.enum_declarations.contains_key(name) {
                    return Ok(named_type(name, NamedCategory::Enum));
                }
                if self.extern_types.contains(name) {
                    return Ok(Type::Ptr(name.to_string()));
                }
                // Check for type def (named function types)
                if let Some(fn_type) = self.builtins.get(name) {
                    if matches!(fn_type, Type::Function(_)) {
                        return Ok(fn_type.clone());
                    }
                }
                Err(CheckError::new(format!("Unknown type '{}'", name)))
            }
        }
    }

    pub(super) fn function_type_from_decl(
        &self,
        fd: &FunctionDeclaration,
    ) -> Result<Type, CheckError> {
        let params: Vec<FunctionParam> = fd
            .params
            .iter()
            .map(|p| {
                let ty = self.resolve_type_node(&p.type_node)?;
                Ok(FunctionParam {
                    name: p.name.clone(),
                    ty,
                    has_default: p.default_value.is_some(),
                    is_mutable: p.is_mutable,
                })
            })
            .collect::<Result<_, CheckError>>()?;
        let returns: Vec<Type> = fd
            .return_types
            .iter()
            .map(|rd| self.resolve_type_node(&rd.type_node))
            .collect::<Result<_, _>>()?;
        let type_params: Vec<String> = fd.type_params.iter().map(|tp| tp.name.clone()).collect();
        Ok(Type::Function(FunctionSig {
            name: fd.name.clone(),
            type_params,
            params,
            returns,
        }))
    }

    pub(super) fn attach_location(&self, err: CheckError, loc: &SourceLocation) -> CheckError {
        if err.file.is_some() {
            return err;
        }
        let file = self.current_file.as_deref().unwrap_or("<unknown>");
        err.with_location(file, loc.line, loc.column)
    }
}

pub(super) fn apply_generic_bindings(ty: &Type, bindings: &HashMap<String, Type>) -> Type {
    match ty {
        Type::TypeParameter(name) => bindings.get(name).cloned().unwrap_or_else(|| ty.clone()),
        Type::Array(inner) => array_of(apply_generic_bindings(inner, bindings)),
        Type::Optional(inner) => optional_of(apply_generic_bindings(inner, bindings)),
        Type::Tuple(items) => tuple_of(
            items
                .iter()
                .map(|i| apply_generic_bindings(i, bindings))
                .collect(),
        ),
        Type::Function(sig) => function_type(
            &sig.name,
            sig.params
                .iter()
                .map(|p| FunctionParam {
                    name: p.name.clone(),
                    ty: apply_generic_bindings(&p.ty, bindings),
                    has_default: p.has_default,
                    is_mutable: p.is_mutable,
                })
                .collect(),
            sig.returns
                .iter()
                .map(|r| apply_generic_bindings(r, bindings))
                .collect(),
        ),
        Type::Named {
            name,
            category,
            generic_bindings: existing,
        } => {
            // Merge current bindings into the Named type's bindings
            let mut merged = existing.clone();
            for (k, v) in bindings {
                if !merged.contains_key(k) {
                    merged.insert(k.clone(), v.clone());
                }
            }
            if merged.is_empty() && existing.is_empty() {
                ty.clone()
            } else {
                Type::Named {
                    name: name.clone(),
                    category: category.clone(),
                    generic_bindings: merged,
                }
            }
        }
        _ => ty.clone(),
    }
}

pub(super) fn is_numeric(ty: &Type) -> bool {
    same_type(ty, &Type::Int) || same_type(ty, &Type::Float)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_numeric_int() {
        assert!(is_numeric(&Type::Int));
    }

    #[test]
    fn test_is_numeric_float() {
        assert!(is_numeric(&Type::Float));
    }

    #[test]
    fn test_is_numeric_string() {
        assert!(!is_numeric(&Type::String));
    }

    #[test]
    fn test_is_numeric_bool() {
        assert!(!is_numeric(&Type::Bool));
    }

    #[test]
    fn test_apply_bindings_substitutes_type_param() {
        let mut bindings = HashMap::new();
        bindings.insert("T".to_string(), Type::Int);
        let result = apply_generic_bindings(&type_parameter("T"), &bindings);
        assert!(matches!(result, Type::Int));
    }

    #[test]
    fn test_apply_bindings_leaves_unbound_alone() {
        let bindings = HashMap::new();
        let result = apply_generic_bindings(&type_parameter("T"), &bindings);
        assert!(matches!(result, Type::TypeParameter(_)));
    }

    #[test]
    fn test_apply_bindings_array() {
        let mut bindings = HashMap::new();
        bindings.insert("T".to_string(), Type::String);
        let result = apply_generic_bindings(&array_of(type_parameter("T")), &bindings);
        match result {
            Type::Array(inner) => assert!(matches!(*inner, Type::String)),
            _ => panic!("expected Array"),
        }
    }

    #[test]
    fn test_apply_bindings_optional() {
        let mut bindings = HashMap::new();
        bindings.insert("T".to_string(), Type::Bool);
        let result = apply_generic_bindings(&optional_of(type_parameter("T")), &bindings);
        match result {
            Type::Optional(inner) => assert!(matches!(*inner, Type::Bool)),
            _ => panic!("expected Optional"),
        }
    }

    #[test]
    fn test_apply_bindings_tuple() {
        let mut bindings = HashMap::new();
        bindings.insert("T".to_string(), Type::Int);
        bindings.insert("U".to_string(), Type::Float);
        let result = apply_generic_bindings(
            &tuple_of(vec![type_parameter("T"), type_parameter("U")]),
            &bindings,
        );
        match result {
            Type::Tuple(items) => {
                assert!(matches!(items[0], Type::Int));
                assert!(matches!(items[1], Type::Float));
            }
            _ => panic!("expected Tuple"),
        }
    }

    #[test]
    fn test_apply_bindings_function() {
        let mut bindings = HashMap::new();
        bindings.insert("T".to_string(), Type::String);
        let func = function_type(
            "f",
            vec![param("x", type_parameter("T"))],
            vec![type_parameter("T")],
        );
        let result = apply_generic_bindings(&func, &bindings);
        match result {
            Type::Function(sig) => {
                assert!(matches!(sig.params[0].ty, Type::String));
                assert!(matches!(sig.returns[0], Type::String));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_apply_bindings_preserves_concrete_types() {
        let bindings = HashMap::new();
        let result = apply_generic_bindings(&Type::Int, &bindings);
        assert!(matches!(result, Type::Int));
    }

    #[test]
    fn test_apply_bindings_named_type_preserves_without_bindings() {
        let bindings = HashMap::new();
        let nt = named_type("MyType", NamedCategory::Type);
        let result = apply_generic_bindings(&nt, &bindings);
        match result {
            Type::Named { name, .. } => assert_eq!(name, "MyType"),
            _ => panic!("expected Named"),
        }
    }

    #[test]
    fn test_apply_bindings_named_type_merges() {
        let mut bindings = HashMap::new();
        bindings.insert("T".to_string(), Type::Int);
        let nt = named_type_with_bindings("Box", NamedCategory::Type, HashMap::new());
        let result = apply_generic_bindings(&nt, &bindings);
        match result {
            Type::Named {
                generic_bindings, ..
            } => {
                assert!(generic_bindings.contains_key("T"));
            }
            _ => panic!("expected Named"),
        }
    }

    // ── Checker method tests ──────────────────────────────────────

    #[test]
    fn test_resolve_base_type_primitives() {
        let c = Checker::new();
        assert!(matches!(
            c.resolve_base_type(Some("Int"), false).unwrap(),
            Type::Int
        ));
        assert!(matches!(
            c.resolve_base_type(Some("Float"), false).unwrap(),
            Type::Float
        ));
        assert!(matches!(
            c.resolve_base_type(Some("String"), false).unwrap(),
            Type::String
        ));
        assert!(matches!(
            c.resolve_base_type(Some("Bool"), false).unwrap(),
            Type::Bool
        ));
        assert!(matches!(
            c.resolve_base_type(Some("Dictionary"), false).unwrap(),
            Type::Dictionary
        ));
        assert!(matches!(
            c.resolve_base_type(Some("Unknown"), false).unwrap(),
            Type::Unknown
        ));
        assert!(matches!(
            c.resolve_base_type(Some("Error"), false).unwrap(),
            Type::Error
        ));
        assert!(matches!(
            c.resolve_base_type(Some("Void"), false).unwrap(),
            Type::Void
        ));
    }

    #[test]
    fn test_resolve_base_type_ptr() {
        let c = Checker::new();
        match c.resolve_base_type(Some("Ptr"), false).unwrap() {
            Type::Ptr(name) => assert_eq!(name, "Ptr"),
            _ => panic!("expected Ptr"),
        }
    }

    #[test]
    fn test_resolve_base_type_type_parameter() {
        let c = Checker::new();
        match c.resolve_base_type(Some("T"), true).unwrap() {
            Type::TypeParameter(name) => assert_eq!(name, "T"),
            _ => panic!("expected TypeParameter"),
        }
    }

    #[test]
    fn test_resolve_base_type_unknown() {
        let c = Checker::new();
        let err = c.resolve_base_type(Some("NotAType"), false);
        assert!(err.is_err());
    }

    #[test]
    fn test_resolve_base_type_missing_name() {
        let c = Checker::new();
        let err = c.resolve_base_type(None, false);
        assert!(err.is_err());
    }

    #[test]
    fn test_resolve_base_type_builtin_function() {
        let c = Checker::new();
        // 'print' is a builtin function registered in builtins
        match c.resolve_base_type(Some("print"), false).unwrap() {
            Type::Function(_) => {}
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_attach_location_preserves_existing_file() {
        let c = Checker::new();
        let err = CheckError::new("x").with_location("a.fai", 1, 2);
        let loc = SourceLocation { line: 5, column: 6 };
        let out = c.attach_location(err, &loc);
        assert_eq!(out.file.as_deref(), Some("a.fai"));
        assert_eq!(out.line, Some(1));
    }

    #[test]
    fn test_attach_location_uses_current_file() {
        let mut c = Checker::new();
        c.current_file = Some("main.fai".to_string());
        let err = CheckError::new("x");
        let loc = SourceLocation {
            line: 10,
            column: 3,
        };
        let out = c.attach_location(err, &loc);
        assert_eq!(out.file.as_deref(), Some("main.fai"));
        assert_eq!(out.line, Some(10));
        assert_eq!(out.column, Some(3));
    }

    #[test]
    fn test_attach_location_defaults_to_unknown() {
        let c = Checker::new();
        let err = CheckError::new("x");
        let loc = SourceLocation { line: 1, column: 1 };
        let out = c.attach_location(err, &loc);
        assert_eq!(out.file.as_deref(), Some("<unknown>"));
    }
}
