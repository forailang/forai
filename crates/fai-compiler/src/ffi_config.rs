//! FFI configuration parsed from `fai.toml` `[ffi.*]` sections.
//!
//! Example fai.toml:
//! ```toml
//! [ffi.sqlite]
//! lib = "sqlite3"
//!
//! [ffi.curl]
//! lib = "curl"
//! ```

use std::collections::HashMap;

/// Configuration for a single FFI library binding.
#[derive(Debug, Clone, PartialEq)]
pub struct FfiLibConfig {
    /// The extern block name (e.g. "sqlite").
    pub name: String,
    /// The C library name to link (e.g. "sqlite3" → -lsqlite3).
    pub lib: String,
}

/// All FFI configuration from a fai.toml file.
#[derive(Debug, Clone, Default)]
pub struct FfiConfig {
    pub libraries: HashMap<String, FfiLibConfig>,
}

/// Parse `[ffi.*]` sections from fai.toml content.
///
/// Recognizes sections like `[ffi.sqlite]` with a `lib = "sqlite3"` key.
pub fn parse_ffi_config(content: &str) -> FfiConfig {
    let mut config = FfiConfig::default();
    let mut current_ffi_name: Option<String> = None;
    let mut current_lib: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // Flush previous section
            if let (Some(name), Some(lib)) = (current_ffi_name.take(), current_lib.take()) {
                config.libraries.insert(
                    name.clone(),
                    FfiLibConfig {
                        name: name.clone(),
                        lib,
                    },
                );
            }

            let section = &trimmed[1..trimmed.len() - 1];
            if let Some(ffi_name) = section.strip_prefix("ffi.") {
                let ffi_name = ffi_name.trim();
                if !ffi_name.is_empty() {
                    current_ffi_name = Some(ffi_name.to_string());
                    current_lib = None;
                }
            } else {
                current_ffi_name = None;
                current_lib = None;
            }
            continue;
        }

        if current_ffi_name.is_some() {
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim();
                let value = value.trim().trim_matches('"');
                if key == "lib" {
                    current_lib = Some(value.to_string());
                }
            }
        }
    }

    // Flush last section
    if let (Some(name), Some(lib)) = (current_ffi_name, current_lib) {
        config.libraries.insert(
            name.clone(),
            FfiLibConfig {
                name: name.clone(),
                lib,
            },
        );
    }

    config
}

/// Load FFI config from a fai.toml file at the given source root.
pub fn load_ffi_config(source_root: &str) -> FfiConfig {
    let src_path = std::path::Path::new(source_root);
    let toml_path = if src_path.join("fai.toml").exists() {
        src_path.join("fai.toml")
    } else if let Some(parent) = src_path.parent() {
        parent.join("fai.toml")
    } else {
        src_path.join("fai.toml")
    };
    match std::fs::read_to_string(&toml_path) {
        Ok(content) => parse_ffi_config(&content),
        Err(_) => FfiConfig::default(),
    }
}

/// Check if a C library can be found on the system.
///
/// Tries in order:
/// 1. `pkg-config --exists <lib>`
/// 2. Common system paths for lib<name>.so / lib<name>.dylib / lib<name>.a
/// 3. Homebrew prefix on macOS
pub fn discover_library(lib_name: &str) -> bool {
    // 1. Try pkg-config
    if let Ok(status) = std::process::Command::new("pkg-config")
        .args(["--exists", lib_name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        if status.success() {
            return true;
        }
    }

    // 2. Check common system paths
    let extensions = if cfg!(target_os = "macos") {
        &["dylib", "a"][..]
    } else {
        &["so", "a"][..]
    };

    let search_paths = [
        "/usr/lib",
        "/usr/local/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
    ];

    for dir in &search_paths {
        for ext in extensions {
            let path = format!("{}/lib{}.{}", dir, lib_name, ext);
            if std::path::Path::new(&path).exists() {
                return true;
            }
        }
    }

    // 3. Try Homebrew on macOS
    if cfg!(target_os = "macos") {
        if let Ok(output) = std::process::Command::new("brew")
            .args(["--prefix", lib_name])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
        {
            if output.status.success() {
                let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
                for ext in extensions {
                    let path = format!("{}/lib/lib{}.{}", prefix, lib_name, ext);
                    if std::path::Path::new(&path).exists() {
                        return true;
                    }
                }
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_toml() {
        let config = parse_ffi_config("");
        assert!(config.libraries.is_empty());
    }

    #[test]
    fn test_parse_no_ffi_sections() {
        let config = parse_ffi_config(
            r#"
[project]
name = "test"
version = "0.1.0"

[dependencies]
"#,
        );
        assert!(config.libraries.is_empty());
    }

    #[test]
    fn test_parse_single_ffi_section() {
        let config = parse_ffi_config(
            r#"
[project]
name = "test"

[ffi.sqlite]
lib = "sqlite3"
"#,
        );
        assert_eq!(config.libraries.len(), 1);
        let sqlite = config.libraries.get("sqlite").unwrap();
        assert_eq!(sqlite.name, "sqlite");
        assert_eq!(sqlite.lib, "sqlite3");
    }

    #[test]
    fn test_parse_multiple_ffi_sections() {
        let config = parse_ffi_config(
            r#"
[project]
name = "test"

[ffi.sqlite]
lib = "sqlite3"

[ffi.curl]
lib = "curl"
"#,
        );
        assert_eq!(config.libraries.len(), 2);

        let sqlite = config.libraries.get("sqlite").unwrap();
        assert_eq!(sqlite.lib, "sqlite3");

        let curl = config.libraries.get("curl").unwrap();
        assert_eq!(curl.lib, "curl");
    }

    #[test]
    fn test_parse_ffi_mixed_with_other_sections() {
        let config = parse_ffi_config(
            r#"
[project]
name = "myapp"
version = "1.0"

[dependencies]
"file:///some/path" = "1.0"

[ffi.sqlite]
lib = "sqlite3"

[build]
opt = "release"
"#,
        );
        assert_eq!(config.libraries.len(), 1);
        assert!(config.libraries.contains_key("sqlite"));
    }

    #[test]
    fn test_parse_ffi_section_without_lib_key() {
        let config = parse_ffi_config(
            r#"
[ffi.sqlite]
something_else = "value"
"#,
        );
        assert!(config.libraries.is_empty());
    }

    #[test]
    fn test_discover_library_nonexistent() {
        assert!(!discover_library("fai_nonexistent_lib_xyz_123"));
    }
}
