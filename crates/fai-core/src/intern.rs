//! String interning table.
//!
//! All identifiers and string literals are interned. An InternedString is a
//! u32 index into the table, enabling O(1) equality checks and efficient
//! dictionary keys.

use std::collections::HashMap;

/// An interned string ID — a u32 index into the intern table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InternedString(pub u32);

impl InternedString {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The string intern table. Deduplicates strings and assigns each a u32 ID.
pub struct InternTable {
    /// Map from string content to its interned ID.
    map: HashMap<String, InternedString>,
    /// All interned strings, indexed by InternedString.0.
    strings: Vec<String>,
}

impl InternTable {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            strings: Vec::new(),
        }
    }

    /// Intern a string, returning its ID. If already interned, returns the existing ID.
    pub fn intern(&mut self, s: &str) -> InternedString {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = InternedString(self.strings.len() as u32);
        self.strings.push(s.to_owned());
        self.map.insert(s.to_owned(), id);
        id
    }

    /// Look up the string for a given interned ID.
    pub fn resolve(&self, id: InternedString) -> &str {
        &self.strings[id.index()]
    }

    /// Number of interned strings.
    pub fn len(&self) -> usize {
        self.strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

impl Default for InternTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intern_dedup() {
        let mut table = InternTable::new();
        let a = table.intern("hello");
        let b = table.intern("hello");
        let c = table.intern("world");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(table.resolve(a), "hello");
        assert_eq!(table.resolve(c), "world");
        assert_eq!(table.len(), 2);
    }
}
