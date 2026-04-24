//! Stack-based scope chain for type checking.

use std::collections::HashMap;

use crate::error::CheckError;
use crate::types::{describe_type, is_assignable, Type};

#[derive(Debug, Clone)]
pub struct ValueBinding {
    pub ty: Type,
    pub mutable: bool,
}

/// A stack of scopes for lexical name resolution.
pub struct Environment {
    scopes: Vec<HashMap<String, ValueBinding>>,
}

impl Environment {
    pub fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub fn define(&mut self, name: &str, ty: Type, mutable: bool) -> Result<(), CheckError> {
        let scope = self.scopes.last_mut().unwrap();
        if scope.contains_key(name) {
            return Err(CheckError::new(format!("Duplicate name '{}'", name)));
        }
        scope.insert(name.to_string(), ValueBinding { ty, mutable });
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<&ValueBinding, CheckError> {
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.get(name) {
                return Ok(binding);
            }
        }
        Err(CheckError::new(format!("Unknown name '{}'", name)))
    }

    pub fn assign(&self, name: &str, ty: &Type) -> Result<(), CheckError> {
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.get(name) {
                if !binding.mutable {
                    return Err(CheckError::new(format!(
                        "Cannot assign to immutable name '{}'",
                        name
                    )));
                }
                if !is_assignable(ty, &binding.ty) {
                    return Err(CheckError::new(format!(
                        "Cannot assign {} to {} '{}'",
                        describe_type(ty),
                        describe_type(&binding.ty),
                        name
                    )));
                }
                return Ok(());
            }
        }
        Err(CheckError::new(format!("Unknown name '{}'", name)))
    }
}
