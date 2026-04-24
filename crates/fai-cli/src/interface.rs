//! Remote interface spec generation.
//!
//! Extracts public function signatures, types, and enums from a forai program
//! and serializes them to JSON. Used by `forai interface` and `forai build` when
//! `[remote-interface] expose = true` is set in fai.toml.

use fai_compiler::ast::*;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// A serializable interface spec.
#[derive(Debug)]
pub struct InterfaceSpec {
    pub name: String,
    pub version: String,
    pub hash: String,
    pub functions: Vec<FunctionSpec>,
    pub types: Vec<TypeSpec>,
    pub enums: Vec<EnumSpec>,
}

#[derive(Debug)]
pub struct FunctionSpec {
    pub name: String,
    pub type_params: Vec<String>,
    pub params: Vec<ParamSpec>,
    pub returns: Vec<String>,
    pub doc_comment: Option<String>,
}

#[derive(Debug)]
pub struct ParamSpec {
    pub name: String,
    pub type_name: String,
    pub has_default: bool,
}

#[derive(Debug)]
pub struct TypeSpec {
    pub name: String,
    pub type_params: Vec<String>,
    pub fields: Vec<FieldSpec>,
}

#[derive(Debug)]
pub struct FieldSpec {
    pub name: String,
    pub type_name: String,
    pub has_default: bool,
    /// Serialization alias — the wire name for this field (`alias: "key"`).
    pub alias: Option<String>,
    /// If true, this field is excluded from serialization/deserialization.
    pub omit: bool,
}

#[derive(Debug)]
pub struct EnumSpec {
    pub name: String,
    pub members: Vec<String>,
}

/// Extract interface spec from a parsed program's statements.
pub fn extract_interface(name: &str, version: &str, statements: &[Statement]) -> InterfaceSpec {
    let mut functions = Vec::new();
    let mut types = Vec::new();
    let mut enums = Vec::new();

    for stmt in statements {
        match stmt {
            Statement::FunctionDeclaration(fd) => {
                let is_private = fd.is_private.unwrap_or(false);
                if !is_private && fd.name != "main" {
                    functions.push(extract_function(fd));
                }
            }
            Statement::TypeDeclaration(td) => {
                let is_private = td.is_private.unwrap_or(false);
                if !is_private {
                    types.push(extract_type(td));
                }
            }
            Statement::EnumDeclaration(ed) => {
                let is_private = ed.is_private.unwrap_or(false);
                if !is_private {
                    enums.push(extract_enum(ed));
                }
            }
            _ => {}
        }
    }

    let hash = compute_hash(&functions, &types, &enums);

    InterfaceSpec {
        name: name.to_string(),
        version: version.to_string(),
        hash,
        functions,
        types,
        enums,
    }
}

/// Extract only `remote`-marked functions and types from a program.
/// Used to generate the schema that clients consume.
pub fn extract_remote_schema(statements: &[Statement]) -> InterfaceSpec {
    let mut functions = Vec::new();
    let mut types = Vec::new();
    let enums = Vec::new();

    for stmt in statements {
        match stmt {
            Statement::FunctionDeclaration(fd) => {
                if fd.is_remote && fd.name != "main" {
                    functions.push(extract_function(fd));
                }
            }
            Statement::TypeDeclaration(td) => {
                if td.is_remote {
                    types.push(extract_type(td));
                }
            }
            _ => {}
        }
    }

    let hash = compute_hash(&functions, &types, &enums);

    InterfaceSpec {
        name: String::new(),
        version: String::new(),
        hash,
        functions,
        types,
        enums,
    }
}

fn extract_function(fd: &FunctionDeclaration) -> FunctionSpec {
    let params = fd
        .params
        .iter()
        .map(|p| ParamSpec {
            name: p.name.clone(),
            type_name: type_node_to_string(&p.type_node),
            has_default: p.default_value.is_some(),
        })
        .collect();

    let returns = fd
        .return_types
        .iter()
        .map(|r| type_node_to_string(&r.type_node))
        .collect();

    FunctionSpec {
        name: fd.name.clone(),
        type_params: fd.type_params.iter().map(|tp| tp.name.clone()).collect(),
        params,
        returns,
        doc_comment: fd.doc_comment.clone(),
    }
}

fn extract_type(td: &TypeDeclaration) -> TypeSpec {
    let fields = td
        .fields
        .iter()
        .map(|f| {
            let alias = f
                .attributes
                .iter()
                .find(|a| a.key == "alias")
                .and_then(|a| a.string_value.clone());
            let omit = f.attributes.iter().any(|a| a.key == "omit");
            FieldSpec {
                name: f.name.clone(),
                type_name: type_node_to_string(&f.type_node),
                has_default: f.default_value.is_some(),
                alias,
                omit,
            }
        })
        .collect();

    TypeSpec {
        name: td.name.clone(),
        type_params: td.type_params.iter().map(|tp| tp.name.clone()).collect(),
        fields,
    }
}

fn extract_enum(ed: &EnumDeclaration) -> EnumSpec {
    EnumSpec {
        name: ed.name.clone(),
        members: ed.members.clone(),
    }
}

fn type_node_to_string(tn: &TypeNode) -> String {
    if let Some(ref params) = tn.function_params {
        let param_strs: Vec<String> = params.iter().map(type_node_to_string).collect();
        let ret_strs: Vec<String> = tn
            .function_returns
            .as_ref()
            .map(|r| r.iter().map(type_node_to_string).collect())
            .unwrap_or_default();
        let base = format!("({}) -> {}", param_strs.join(", "), ret_strs.join(", "));
        return maybe_optional_array(base, tn.is_array, tn.is_optional);
    }

    let base = tn.name.as_deref().unwrap_or("Unknown").to_string();
    maybe_optional_array(base, tn.is_array, tn.is_optional)
}

fn maybe_optional_array(mut s: String, is_array: bool, is_optional: bool) -> String {
    if is_array {
        s.push_str("[]");
    }
    if is_optional {
        s.push('?');
    }
    s
}

fn compute_hash(functions: &[FunctionSpec], types: &[TypeSpec], enums: &[EnumSpec]) -> String {
    let mut hasher = DefaultHasher::new();

    for f in functions {
        f.name.hash(&mut hasher);
        for p in &f.params {
            p.name.hash(&mut hasher);
            p.type_name.hash(&mut hasher);
        }
        for r in &f.returns {
            r.hash(&mut hasher);
        }
    }
    for t in types {
        t.name.hash(&mut hasher);
        for f in &t.fields {
            f.name.hash(&mut hasher);
            f.type_name.hash(&mut hasher);
            // alias changes the wire format — treat as a breaking change
            f.alias.hash(&mut hasher);
            f.omit.hash(&mut hasher);
        }
    }
    for e in enums {
        e.name.hash(&mut hasher);
        for m in &e.members {
            m.hash(&mut hasher);
        }
    }

    format!("{:x}", hasher.finish())
}

/// Serialize interface spec to JSON string.
pub fn spec_to_json(spec: &InterfaceSpec) -> String {
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!("  \"name\": \"{}\",\n", spec.name));
    json.push_str(&format!("  \"version\": \"{}\",\n", spec.version));
    json.push_str(&format!("  \"hash\": \"{}\",\n", spec.hash));

    // Functions
    json.push_str("  \"functions\": [\n");
    for (i, f) in spec.functions.iter().enumerate() {
        json.push_str("    {\n");
        json.push_str(&format!("      \"name\": \"{}\",\n", f.name));
        if !f.type_params.is_empty() {
            let tps: Vec<String> = f.type_params.iter().map(|t| format!("\"{}\"", t)).collect();
            json.push_str(&format!("      \"typeParams\": [{}],\n", tps.join(", ")));
        }
        json.push_str("      \"params\": [");
        for (j, p) in f.params.iter().enumerate() {
            json.push_str(&format!(
                "{{\"name\":\"{}\",\"type\":\"{}\",\"hasDefault\":{}}}",
                p.name, p.type_name, p.has_default
            ));
            if j < f.params.len() - 1 {
                json.push_str(", ");
            }
        }
        json.push_str("],\n");
        let rets: Vec<String> = f.returns.iter().map(|r| format!("\"{}\"", r)).collect();
        json.push_str(&format!("      \"returns\": [{}]\n", rets.join(", ")));
        json.push_str("    }");
        if i < spec.functions.len() - 1 {
            json.push(',');
        }
        json.push('\n');
    }
    json.push_str("  ],\n");

    // Types
    json.push_str("  \"types\": [\n");
    for (i, t) in spec.types.iter().enumerate() {
        json.push_str("    {\n");
        json.push_str(&format!("      \"name\": \"{}\",\n", t.name));
        if !t.type_params.is_empty() {
            let tps: Vec<String> = t
                .type_params
                .iter()
                .map(|tp| format!("\"{}\"", tp))
                .collect();
            json.push_str(&format!("      \"typeParams\": [{}],\n", tps.join(", ")));
        }
        json.push_str("      \"fields\": [");
        for (j, f) in t.fields.iter().enumerate() {
            let mut field_json = format!(
                "{{\"name\":\"{}\",\"type\":\"{}\",\"hasDefault\":{}",
                f.name, f.type_name, f.has_default
            );
            if let Some(ref alias) = f.alias {
                field_json.push_str(&format!(",\"alias\":\"{}\"", alias));
            }
            if f.omit {
                field_json.push_str(",\"omit\":true");
            }
            field_json.push('}');
            json.push_str(&field_json);
            if j < t.fields.len() - 1 {
                json.push_str(", ");
            }
        }
        json.push_str("]\n");
        json.push_str("    }");
        if i < spec.types.len() - 1 {
            json.push(',');
        }
        json.push('\n');
    }
    json.push_str("  ],\n");

    // Enums
    json.push_str("  \"enums\": [\n");
    for (i, e) in spec.enums.iter().enumerate() {
        let members: Vec<String> = e.members.iter().map(|m| format!("\"{}\"", m)).collect();
        json.push_str(&format!(
            "    {{\"name\":\"{}\",\"members\":[{}]}}",
            e.name,
            members.join(", ")
        ));
        if i < spec.enums.len() - 1 {
            json.push(',');
        }
        json.push('\n');
    }
    json.push_str("  ]\n");

    json.push('}');
    json
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc() -> SourceLocation {
        SourceLocation { line: 1, column: 1 }
    }

    fn simple_type(name: &str) -> TypeNode {
        TypeNode {
            kind: "TypeNode".to_string(),
            name: Some(name.to_string()),
            is_type_parameter: None,
            function_params: None,
            function_returns: None,
            is_array: false,
            is_optional: false,
            location: loc(),
        }
    }

    fn make_param(name: &str, type_name: &str) -> Parameter {
        Parameter {
            name: name.to_string(),
            type_node: simple_type(type_name),
            default_value: None,
            is_out: false,
            is_mutable: false,
            location: loc(),
            doc_comment: None,
        }
    }

    fn make_return(type_name: &str) -> ReturnDeclaration {
        ReturnDeclaration {
            name: None,
            type_node: simple_type(type_name),
            doc_comment: None,
            location: loc(),
        }
    }

    fn make_fn(name: &str, is_private: Option<bool>) -> FunctionDeclaration {
        FunctionDeclaration {
            name: name.to_string(),
            type_params: vec![],
            params: vec![],
            return_types: vec![],
            body: vec![],
            doc: None,
            is_private,
            is_abstract: false,
            is_remote: false,
            location: loc(),
            doc_comment: None,
        }
    }

    fn make_type(name: &str, is_private: Option<bool>) -> TypeDeclaration {
        TypeDeclaration {
            name: name.to_string(),
            type_params: vec![],
            fields: vec![],
            doc: None,
            is_private,
            is_remote: false,
            location: loc(),
        }
    }

    fn make_remote_fn(name: &str) -> FunctionDeclaration {
        let mut fd = make_fn(name, None);
        fd.is_remote = true;
        fd
    }

    fn make_remote_type(name: &str) -> TypeDeclaration {
        let mut td = make_type(name, None);
        td.is_remote = true;
        td
    }

    fn make_enum(name: &str, is_private: Option<bool>, members: &[&str]) -> EnumDeclaration {
        EnumDeclaration {
            name: name.to_string(),
            members: members.iter().map(|s| s.to_string()).collect(),
            doc: None,
            is_private,
            location: loc(),
        }
    }

    fn type_param(name: &str) -> TypeParamDeclaration {
        TypeParamDeclaration {
            name: name.to_string(),
            doc_comment: None,
            location: loc(),
        }
    }

    fn make_field(name: &str, type_name: &str) -> FieldDeclaration {
        FieldDeclaration {
            name: name.to_string(),
            type_node: simple_type(type_name),
            default_value: None,
            attributes: vec![],
            location: loc(),
        }
    }

    fn make_field_with_alias(name: &str, type_name: &str, alias: &str) -> FieldDeclaration {
        FieldDeclaration {
            name: name.to_string(),
            type_node: simple_type(type_name),
            default_value: None,
            attributes: vec![FieldAttribute {
                key: "alias".to_string(),
                string_value: Some(alias.to_string()),
            }],
            location: loc(),
        }
    }

    fn make_field_omit(name: &str, type_name: &str) -> FieldDeclaration {
        FieldDeclaration {
            name: name.to_string(),
            type_node: simple_type(type_name),
            default_value: None,
            attributes: vec![FieldAttribute {
                key: "omit".to_string(),
                string_value: None,
            }],
            location: loc(),
        }
    }

    // ── extract_interface ────────────────────────────────────────────

    #[test]
    fn test_empty_interface() {
        let spec = extract_interface("myapp", "1.0.0", &[]);
        assert_eq!(spec.name, "myapp");
        assert_eq!(spec.version, "1.0.0");
        assert!(spec.functions.is_empty());
        assert!(spec.types.is_empty());
        assert!(spec.enums.is_empty());
    }

    #[test]
    fn test_function_extraction() {
        let mut fd = make_fn("greet", None);
        fd.params.push(make_param("name", "String"));
        fd.return_types.push(make_return("String"));
        let stmts = vec![Statement::FunctionDeclaration(fd)];
        let spec = extract_interface("app", "1.0.0", &stmts);

        assert_eq!(spec.functions.len(), 1);
        let f = &spec.functions[0];
        assert_eq!(f.name, "greet");
        assert_eq!(f.params[0].name, "name");
        assert_eq!(f.params[0].type_name, "String");
        assert!(!f.params[0].has_default);
        assert_eq!(f.returns, vec!["String"]);
    }

    #[test]
    fn test_private_function_excluded() {
        let stmts = vec![Statement::FunctionDeclaration(make_fn(
            "internal",
            Some(true),
        ))];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert!(spec.functions.is_empty());
    }

    #[test]
    fn test_main_excluded() {
        let stmts = vec![Statement::FunctionDeclaration(make_fn("main", None))];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert!(spec.functions.is_empty());
    }

    #[test]
    fn test_explicit_public_function_included() {
        let stmts = vec![Statement::FunctionDeclaration(make_fn(
            "visible",
            Some(false),
        ))];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert_eq!(spec.functions.len(), 1);
    }

    #[test]
    fn test_type_extraction() {
        let mut td = make_type("User", None);
        td.fields.push(FieldDeclaration {
            name: "name".to_string(),
            type_node: simple_type("String"),
            default_value: None,
            attributes: vec![],
            location: loc(),
        });
        td.fields.push(FieldDeclaration {
            name: "age".to_string(),
            type_node: simple_type("Int"),
            default_value: None,
            attributes: vec![],
            location: loc(),
        });
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);

        assert_eq!(spec.types.len(), 1);
        let t = &spec.types[0];
        assert_eq!(t.name, "User");
        assert_eq!(t.fields.len(), 2);
        assert_eq!(t.fields[0].name, "name");
        assert_eq!(t.fields[0].type_name, "String");
        assert!(!t.fields[0].has_default);
        assert_eq!(t.fields[1].type_name, "Int");
    }

    #[test]
    fn test_private_type_excluded() {
        let stmts = vec![Statement::TypeDeclaration(make_type(
            "Internal",
            Some(true),
        ))];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert!(spec.types.is_empty());
    }

    #[test]
    fn test_enum_extraction() {
        let ed = make_enum("Status", None, &["active", "inactive", "banned"]);
        let stmts = vec![Statement::EnumDeclaration(ed)];
        let spec = extract_interface("app", "1.0.0", &stmts);

        assert_eq!(spec.enums.len(), 1);
        assert_eq!(spec.enums[0].name, "Status");
        assert_eq!(spec.enums[0].members, vec!["active", "inactive", "banned"]);
    }

    #[test]
    fn test_private_enum_excluded() {
        let stmts = vec![Statement::EnumDeclaration(make_enum(
            "Hidden",
            Some(true),
            &["a"],
        ))];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert!(spec.enums.is_empty());
    }

    #[test]
    fn test_non_decl_statements_ignored() {
        let ls = LetStatement {
            bindings: vec![],
            value: Expression::NullExpression(NullExpression { location: loc() }),
            is_private: None,
            location: loc(),
        };
        let spec = extract_interface("app", "1.0.0", &[Statement::LetStatement(ls)]);
        assert!(spec.functions.is_empty());
        assert!(spec.types.is_empty());
        assert!(spec.enums.is_empty());
    }

    #[test]
    fn test_function_with_type_params() {
        let mut fd = make_fn("identity", None);
        fd.type_params.push(type_param("T"));
        fd.params.push(Parameter {
            name: "x".to_string(),
            type_node: TypeNode {
                kind: "TypeNode".to_string(),
                name: Some("T".to_string()),
                is_type_parameter: Some(true),
                function_params: None,
                function_returns: None,
                is_array: false,
                is_optional: false,
                location: loc(),
            },
            default_value: None,
            is_out: false,
            is_mutable: false,
            location: loc(),
            doc_comment: None,
        });
        let stmts = vec![Statement::FunctionDeclaration(fd)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert_eq!(spec.functions[0].type_params, vec!["T"]);
    }

    #[test]
    fn test_array_type_node() {
        let mut fd = make_fn("items", None);
        fd.params.push(Parameter {
            name: "xs".to_string(),
            type_node: TypeNode {
                is_array: true,
                ..simple_type("Int")
            },
            default_value: None,
            is_out: false,
            is_mutable: false,
            location: loc(),
            doc_comment: None,
        });
        let stmts = vec![Statement::FunctionDeclaration(fd)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert_eq!(spec.functions[0].params[0].type_name, "Int[]");
    }

    #[test]
    fn test_optional_type_node() {
        let mut fd = make_fn("find", None);
        fd.return_types.push(ReturnDeclaration {
            name: None,
            type_node: TypeNode {
                is_optional: true,
                ..simple_type("String")
            },
            doc_comment: None,
            location: loc(),
        });
        let stmts = vec![Statement::FunctionDeclaration(fd)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert_eq!(spec.functions[0].returns, vec!["String?"]);
    }

    #[test]
    fn test_array_optional_combined() {
        let mut fd = make_fn("find_all", None);
        fd.return_types.push(ReturnDeclaration {
            name: None,
            type_node: TypeNode {
                is_array: true,
                is_optional: true,
                ..simple_type("String")
            },
            doc_comment: None,
            location: loc(),
        });
        let stmts = vec![Statement::FunctionDeclaration(fd)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert_eq!(spec.functions[0].returns, vec!["String[]?"]);
    }

    #[test]
    fn test_function_type_node() {
        let cb_type = TypeNode {
            kind: "TypeNode".to_string(),
            name: None,
            is_type_parameter: None,
            function_params: Some(vec![simple_type("Int")]),
            function_returns: Some(vec![simple_type("Bool")]),
            is_array: false,
            is_optional: false,
            location: loc(),
        };
        let mut fd = make_fn("apply", None);
        fd.params.push(Parameter {
            name: "cb".to_string(),
            type_node: cb_type,
            default_value: None,
            is_out: false,
            is_mutable: false,
            location: loc(),
            doc_comment: None,
        });
        let stmts = vec![Statement::FunctionDeclaration(fd)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert_eq!(spec.functions[0].params[0].type_name, "(Int) -> Bool");
    }

    #[test]
    fn test_function_with_default_param() {
        let mut fd = make_fn("greet", None);
        fd.params.push(Parameter {
            name: "name".to_string(),
            type_node: simple_type("String"),
            default_value: Some(Expression::StringExpression(StringExpression {
                value: "world".to_string(),
                location: loc(),
            })),
            is_out: false,
            is_mutable: false,
            location: loc(),
            doc_comment: None,
        });
        let stmts = vec![Statement::FunctionDeclaration(fd)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert!(spec.functions[0].params[0].has_default);
    }

    #[test]
    fn test_type_with_default_field() {
        let mut td = make_type("Config", None);
        td.fields.push(FieldDeclaration {
            name: "debug".to_string(),
            type_node: simple_type("Bool"),
            default_value: Some(Expression::BooleanExpression(BooleanExpression {
                value: false,
                location: loc(),
            })),
            attributes: vec![],
            location: loc(),
        });
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert!(spec.types[0].fields[0].has_default);
    }

    #[test]
    fn test_type_with_type_params() {
        let mut td = make_type("Box", None);
        td.type_params.push(type_param("T"));
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert_eq!(spec.types[0].type_params, vec!["T"]);
    }

    // ── spec_to_json ─────────────────────────────────────────────────

    #[test]
    fn test_spec_to_json_basic_fields() {
        let spec = extract_interface("myapp", "2.0.0", &[]);
        let json = spec_to_json(&spec);
        assert!(json.contains("\"name\": \"myapp\""));
        assert!(json.contains("\"version\": \"2.0.0\""));
        assert!(json.contains("\"hash\":"));
        assert!(json.contains("\"functions\": ["));
        assert!(json.contains("\"types\": ["));
        assert!(json.contains("\"enums\": ["));
    }

    #[test]
    fn test_spec_to_json_with_function() {
        let mut fd = make_fn("add", None);
        fd.params.push(make_param("a", "Int"));
        fd.params.push(make_param("b", "Int"));
        fd.return_types.push(make_return("Int"));
        let stmts = vec![Statement::FunctionDeclaration(fd)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let json = spec_to_json(&spec);

        assert!(json.contains("\"name\": \"add\""));
        assert!(json.contains("\"name\":\"a\""));
        assert!(json.contains("\"type\":\"Int\""));
        assert!(json.contains("\"returns\": [\"Int\"]"));
    }

    #[test]
    fn test_spec_to_json_function_type_params() {
        let mut fd = make_fn("identity", None);
        fd.type_params.push(type_param("T"));
        let stmts = vec![Statement::FunctionDeclaration(fd)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let json = spec_to_json(&spec);
        assert!(json.contains("\"typeParams\""));
        assert!(json.contains("\"T\""));
    }

    #[test]
    fn test_spec_to_json_with_type() {
        let mut td = make_type("User", None);
        td.type_params.push(type_param("T"));
        td.fields.push(FieldDeclaration {
            name: "value".to_string(),
            type_node: simple_type("String"),
            default_value: None,
            attributes: vec![],
            location: loc(),
        });
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let json = spec_to_json(&spec);
        assert!(json.contains("\"name\": \"User\""));
        assert!(json.contains("\"typeParams\""));
        assert!(json.contains("\"name\":\"value\""));
    }

    #[test]
    fn test_spec_to_json_with_enum() {
        let ed = make_enum("Color", None, &["red", "green", "blue"]);
        let stmts = vec![Statement::EnumDeclaration(ed)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let json = spec_to_json(&spec);
        assert!(json.contains("\"name\":\"Color\""));
        assert!(json.contains("\"red\""));
        assert!(json.contains("\"green\""));
    }

    #[test]
    fn test_spec_to_json_multiple_functions() {
        let stmts = vec![
            Statement::FunctionDeclaration(make_fn("alpha", None)),
            Statement::FunctionDeclaration(make_fn("beta", None)),
        ];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let json = spec_to_json(&spec);
        // First function has no trailing comma issue
        assert!(json.contains("\"name\": \"alpha\""));
        assert!(json.contains("\"name\": \"beta\""));
    }

    // ── compute_hash ─────────────────────────────────────────────────

    #[test]
    fn test_hash_is_deterministic() {
        let ed1 = make_enum("Status", None, &["active"]);
        let spec1 = extract_interface("app", "1.0.0", &[Statement::EnumDeclaration(ed1)]);

        let ed2 = make_enum("Status", None, &["active"]);
        let spec2 = extract_interface("app", "1.0.0", &[Statement::EnumDeclaration(ed2)]);

        assert_eq!(spec1.hash, spec2.hash);
    }

    #[test]
    fn test_hash_differs_for_different_content() {
        let ed1 = make_enum("Status", None, &["active"]);
        let spec1 = extract_interface("app", "1.0.0", &[Statement::EnumDeclaration(ed1)]);

        let ed2 = make_enum("Status", None, &["inactive"]);
        let spec2 = extract_interface("app", "1.0.0", &[Statement::EnumDeclaration(ed2)]);

        assert_ne!(spec1.hash, spec2.hash);
    }

    #[test]
    fn test_hash_is_hex_string() {
        let spec = extract_interface("app", "1.0.0", &[]);
        assert!(spec.hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!spec.hash.is_empty());
    }

    #[test]
    fn test_spec_to_json_type_without_type_params() {
        // Covers the "no type_params" branch (line 234) in spec_to_json
        let td = make_type("Point", None);
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let json = spec_to_json(&spec);
        assert!(json.contains("\"name\": \"Point\""));
        // No typeParams key when type_params is empty
        assert!(!json.contains("typeParams"));
    }

    #[test]
    fn test_multiple_items_order_preserved() {
        let stmts = vec![
            Statement::FunctionDeclaration(make_fn("alpha", None)),
            Statement::FunctionDeclaration(make_fn("beta", None)),
        ];
        let spec = extract_interface("app", "1.0.0", &stmts);
        assert_eq!(spec.functions[0].name, "alpha");
        assert_eq!(spec.functions[1].name, "beta");
    }

    // ── extract_remote_schema tests ───────────────────────────────

    #[test]
    fn test_remote_schema_includes_only_remote_functions() {
        let stmts = vec![
            Statement::FunctionDeclaration(make_fn("helper", None)), // not remote
            Statement::FunctionDeclaration(make_remote_fn("getTasks")), // remote
            Statement::FunctionDeclaration(make_remote_fn("addTask")), // remote
        ];
        let spec = extract_remote_schema(&stmts);
        assert_eq!(spec.functions.len(), 2);
        assert_eq!(spec.functions[0].name, "getTasks");
        assert_eq!(spec.functions[1].name, "addTask");
    }

    #[test]
    fn test_remote_schema_includes_only_remote_types() {
        let stmts = vec![
            Statement::TypeDeclaration(make_type("AppState", None)), // not remote
            Statement::TypeDeclaration(make_remote_type("Task")),    // remote
        ];
        let spec = extract_remote_schema(&stmts);
        assert_eq!(spec.types.len(), 1);
        assert_eq!(spec.types[0].name, "Task");
    }

    #[test]
    fn test_remote_schema_excludes_main() {
        let mut main_fn = make_remote_fn("main");
        main_fn.is_remote = true;
        let stmts = vec![
            Statement::FunctionDeclaration(main_fn),
            Statement::FunctionDeclaration(make_remote_fn("getData")),
        ];
        let spec = extract_remote_schema(&stmts);
        assert_eq!(spec.functions.len(), 1);
        assert_eq!(spec.functions[0].name, "getData");
    }

    #[test]
    fn test_remote_schema_empty_when_no_remote_items() {
        let stmts = vec![
            Statement::FunctionDeclaration(make_fn("helper", None)),
            Statement::TypeDeclaration(make_type("Internal", None)),
        ];
        let spec = extract_remote_schema(&stmts);
        assert!(spec.functions.is_empty());
        assert!(spec.types.is_empty());
    }

    #[test]
    fn test_remote_schema_json_roundtrip() {
        let stmts = vec![
            Statement::TypeDeclaration(make_remote_type("Task")),
            Statement::FunctionDeclaration(make_remote_fn("getTasks")),
        ];
        let spec = extract_remote_schema(&stmts);
        let json = spec_to_json(&spec);
        assert!(json.contains("\"Task\""));
        assert!(json.contains("\"getTasks\""));
        assert!(json.contains("\"hash\""));
    }

    // ── field attributes ─────────────────────────────────────────────

    #[test]
    fn test_field_alias_extracted() {
        let mut td = make_type("User", None);
        td.fields
            .push(make_field_with_alias("userName", "String", "user_name"));
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let field = &spec.types[0].fields[0];
        assert_eq!(field.alias, Some("user_name".to_string()));
        assert!(!field.omit);
    }

    #[test]
    fn test_field_omit_extracted() {
        let mut td = make_type("User", None);
        td.fields.push(make_field_omit("password", "String"));
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let field = &spec.types[0].fields[0];
        assert!(field.omit);
        assert_eq!(field.alias, None);
    }

    #[test]
    fn test_field_no_attributes_defaults() {
        let mut td = make_type("User", None);
        td.fields.push(make_field("name", "String"));
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let field = &spec.types[0].fields[0];
        assert_eq!(field.alias, None);
        assert!(!field.omit);
    }

    #[test]
    fn test_spec_to_json_includes_alias() {
        let mut td = make_type("User", None);
        td.fields
            .push(make_field_with_alias("userName", "String", "user_name"));
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let json = spec_to_json(&spec);
        assert!(
            json.contains("\"alias\":\"user_name\""),
            "JSON should include alias. Got:\n{}",
            json
        );
    }

    #[test]
    fn test_spec_to_json_includes_omit() {
        let mut td = make_type("User", None);
        td.fields.push(make_field_omit("password", "String"));
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let json = spec_to_json(&spec);
        assert!(
            json.contains("\"omit\":true"),
            "JSON should include omit. Got:\n{}",
            json
        );
    }

    #[test]
    fn test_spec_to_json_no_alias_key_when_absent() {
        let mut td = make_type("User", None);
        td.fields.push(make_field("name", "String"));
        let stmts = vec![Statement::TypeDeclaration(td)];
        let spec = extract_interface("app", "1.0.0", &stmts);
        let json = spec_to_json(&spec);
        assert!(
            !json.contains("\"alias\""),
            "JSON should not include alias key when absent"
        );
        assert!(
            !json.contains("\"omit\""),
            "JSON should not include omit key when absent"
        );
    }

    #[test]
    fn test_hash_changes_when_alias_changes() {
        let mut td1 = make_type("User", None);
        td1.fields.push(make_field("userName", "String"));
        let spec1 = extract_interface("app", "1.0.0", &[Statement::TypeDeclaration(td1)]);

        let mut td2 = make_type("User", None);
        td2.fields
            .push(make_field_with_alias("userName", "String", "user_name"));
        let spec2 = extract_interface("app", "1.0.0", &[Statement::TypeDeclaration(td2)]);

        assert_ne!(
            spec1.hash, spec2.hash,
            "adding alias should change the hash"
        );
    }

    #[test]
    fn test_hash_changes_when_omit_added() {
        let mut td1 = make_type("Payload", None);
        td1.fields.push(make_field("secret", "String"));
        let spec1 = extract_interface("app", "1.0.0", &[Statement::TypeDeclaration(td1)]);

        let mut td2 = make_type("Payload", None);
        td2.fields.push(make_field_omit("secret", "String"));
        let spec2 = extract_interface("app", "1.0.0", &[Statement::TypeDeclaration(td2)]);

        assert_ne!(spec1.hash, spec2.hash, "adding omit should change the hash");
    }
}
