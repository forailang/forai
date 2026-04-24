//! Array builtins: HOFs and array manipulation.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(
        b,
        "first",
        &[p("items", array_of(type_parameter("T")))],
        &[optional_of(type_parameter("T"))],
    );
    ins(
        b,
        "last",
        &[p("items", array_of(type_parameter("T")))],
        &[optional_of(type_parameter("T"))],
    );
    ins(
        b,
        "map",
        &[
            p("items", array_of(type_parameter("T"))),
            p(
                "using",
                function_type(
                    "mapper",
                    vec![param("item", type_parameter("T"))],
                    vec![type_parameter("U")],
                ),
            ),
        ],
        &[array_of(type_parameter("U"))],
    );
    ins(
        b,
        "filter",
        &[
            p("items", array_of(type_parameter("T"))),
            p(
                "using",
                function_type(
                    "predicate",
                    vec![param("item", type_parameter("T"))],
                    vec![Type::Bool],
                ),
            ),
        ],
        &[array_of(type_parameter("T"))],
    );
    ins(
        b,
        "find",
        &[
            p("items", array_of(type_parameter("T"))),
            p(
                "using",
                function_type(
                    "predicate",
                    vec![param("item", type_parameter("T"))],
                    vec![Type::Bool],
                ),
            ),
        ],
        &[optional_of(type_parameter("T"))],
    );
    ins(
        b,
        "isAny",
        &[
            p("items", array_of(type_parameter("T"))),
            p(
                "using",
                function_type(
                    "predicate",
                    vec![param("item", type_parameter("T"))],
                    vec![Type::Bool],
                ),
            ),
        ],
        &[Type::Bool],
    );
    ins(
        b,
        "isAll",
        &[
            p("items", array_of(type_parameter("T"))),
            p(
                "using",
                function_type(
                    "predicate",
                    vec![param("item", type_parameter("T"))],
                    vec![Type::Bool],
                ),
            ),
        ],
        &[Type::Bool],
    );
    ins(
        b,
        "arrayContains",
        &[
            p("items", array_of(type_parameter("T"))),
            p("value", type_parameter("T")),
        ],
        &[Type::Bool],
    );
    ins(
        b,
        "arraySort",
        &[p("items", array_of(Type::Unknown))],
        &[array_of(Type::Unknown)],
    );
    ins(
        b,
        "arrayReverse",
        &[p("items", array_of(type_parameter("T")))],
        &[array_of(type_parameter("T"))],
    );
    ins(
        b,
        "arrayIndexOf",
        &[
            p("items", array_of(Type::Unknown)),
            p("value", Type::Unknown),
        ],
        &[Type::Int],
    );
    ins(
        b,
        "arrayJoin",
        &[
            p("items", array_of(Type::String)),
            p("separator", Type::String),
        ],
        &[Type::String],
    );
    ins(
        b,
        "arraySlice",
        &[
            p("items", array_of(type_parameter("T"))),
            p("start", Type::Int),
            p("end", Type::Int),
        ],
        &[array_of(type_parameter("T"))],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> HashMap<String, Type> {
        let mut b = HashMap::new();
        install(&mut b);
        b
    }

    #[test]
    fn test_hof_builtins() {
        let b = fresh();
        for name in &["first", "last", "map", "filter", "find", "isAny", "isAll"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_array_manipulation_builtins() {
        let b = fresh();
        for name in &[
            "arrayContains",
            "arraySort",
            "arrayReverse",
            "arrayIndexOf",
            "arrayJoin",
            "arraySlice",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_map_signature_takes_function_param() {
        let b = fresh();
        match b.get("map").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.params[1].ty, Type::Function(_)));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_array_slice_takes_three_params() {
        let b = fresh();
        match b.get("arraySlice").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 3);
                assert!(matches!(sig.params[1].ty, Type::Int));
                assert!(matches!(sig.params[2].ty, Type::Int));
            }
            _ => panic!("expected Function"),
        }
    }
}
