//! Type checking error type.

/// A type checking error with location information.
#[derive(Debug)]
pub struct CheckError {
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

impl CheckError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            file: None,
            line: None,
            column: None,
        }
    }

    pub fn with_location(mut self, file: &str, line: u32, column: u32) -> Self {
        self.file = Some(file.to_string());
        self.line = Some(line);
        self.column = Some(column);
        self
    }
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "type error: {}", self.message)?;
        if let (Some(file), Some(line), Some(col)) = (&self.file, self.line, self.column) {
            write!(f, "\n  at {}:{}:{}", file, line, col)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_error_new() {
        let e = CheckError::new("something went wrong");
        assert_eq!(e.message, "something went wrong");
        assert!(e.file.is_none());
        assert!(e.line.is_none());
        assert!(e.column.is_none());
    }

    #[test]
    fn test_check_error_with_location() {
        let e = CheckError::new("bad type").with_location("foo.fai", 5, 10);
        assert_eq!(e.file.as_deref(), Some("foo.fai"));
        assert_eq!(e.line, Some(5));
        assert_eq!(e.column, Some(10));
    }

    #[test]
    fn test_display_without_location() {
        let e = CheckError::new("unknown name");
        let s = e.to_string();
        assert!(s.contains("type error:"));
        assert!(s.contains("unknown name"));
        assert!(!s.contains("at "));
    }

    #[test]
    fn test_display_with_location() {
        let e = CheckError::new("bad type").with_location("main.fai", 3, 7);
        let s = e.to_string();
        assert!(s.contains("bad type"));
        assert!(s.contains("main.fai:3:7"));
    }
}
