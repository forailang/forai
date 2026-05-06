//! Dependency URL parsing and resolution for `[dependencies]` entries.
//!
//! Format: `Name = "url"` where `Name` is the canonical package name
//! (must match the dep's own `[project] name`) and `url` is one of:
//!
//! - `file:///abs/path` or `file://relative/path` — local source tree.
//!   Relative paths resolve against the project root (the directory
//!   holding the consumer's `fai.toml`), not the process cwd.
//! - `https://github.com/<owner>/<repo>` — git URL. Cloned shallowly
//!   into `~/.fai/cache/git/<host>/<owner>/<repo>/` on first miss, then
//!   reused. To refresh, delete the cache directory.
//!
//! Future: `Name = { url = "...", path = "<subdir>" }` for multiple
//! packages in one repo, plus `rev`/`tag`/`branch` pinning. v0 always
//! clones the default branch.
//!
//! The historical `"url" = "version"` direction (URL on the left,
//! version string on the right) is no longer accepted.

use std::path::{Path, PathBuf};

/// One parsed dependency line: `Name = "url"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepSpec {
    pub name: String,
    pub url: String,
}

/// Outcome of resolving a dep URL to a local directory containing
/// the dep's `fai.toml`.
#[derive(Debug)]
pub enum ResolveError {
    UnsupportedScheme(String),
    GitNotInstalled,
    GitCloneFailed { url: String, stderr: String },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::UnsupportedScheme(s) => write!(
                f,
                "unsupported dependency URL scheme: {} (expected file:// or https://)",
                s
            ),
            ResolveError::GitNotInstalled => write!(
                f,
                "`git` was not found on PATH — install git to fetch https:// dependencies"
            ),
            ResolveError::GitCloneFailed { url, stderr } => {
                write!(f, "git clone of {} failed: {}", url, stderr.trim())
            }
        }
    }
}

/// Parse one trimmed `[dependencies]` line. Returns `Some(DepSpec)`
/// when the line is a recognized `Name = "url"` entry; returns
/// `None` for blank lines, comments, or malformed input.
pub fn parse_dep_line(trimmed: &str) -> Option<DepSpec> {
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (k, v) = trimmed.split_once('=')?;
    let name = k.trim().trim_matches('"').to_string();
    let url = v.trim().trim_matches('"').to_string();
    if name.is_empty() || url.is_empty() {
        return None;
    }
    // Reject the legacy URL-on-left form so users get a clear signal
    // when they hit the renamed format.
    if name.starts_with("file://") || name.starts_with("https://") || name.starts_with("http://") {
        return None;
    }
    Some(DepSpec { name, url })
}

/// Resolve a dep `url` to a local directory holding its `fai.toml`.
/// `project_root` is the directory containing the *consumer's*
/// `fai.toml`; relative `file://` paths resolve against it.
pub fn resolve_dep_url(url: &str, project_root: &Path) -> Result<PathBuf, ResolveError> {
    if let Some(raw_path) = url.strip_prefix("file://") {
        let p = Path::new(raw_path);
        if p.is_absolute() {
            Ok(PathBuf::from(raw_path))
        } else {
            Ok(project_root.join(raw_path))
        }
    } else if url.starts_with("https://") || url.starts_with("http://") {
        ensure_git_clone(url)
    } else {
        Err(ResolveError::UnsupportedScheme(url.to_string()))
    }
}

/// Where cloned git deps live. Override with `FAI_CACHE_DIR` for
/// tests or sandboxed environments.
fn cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("FAI_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".fai/cache");
    }
    std::env::temp_dir().join("fai-cache")
}

/// Compute the on-disk cache path for a git URL:
/// `<cache_root>/git/<host>/<owner>/<repo>` (with any trailing
/// `.git` stripped).
pub fn git_cache_path_for_url(cache_root: &Path, url: &str) -> PathBuf {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let after_scheme = after_scheme.strip_suffix(".git").unwrap_or(after_scheme);
    cache_root.join("git").join(after_scheme)
}

fn ensure_git_clone(url: &str) -> Result<PathBuf, ResolveError> {
    let root = cache_root();
    let target = git_cache_path_for_url(&root, url);
    // Cache hit: a populated clone already exists.
    if target.join("fai.toml").exists() {
        return Ok(target);
    }
    // Make sure the parent exists, then clone fresh. If a stale,
    // empty target dir exists from a prior partial clone, remove it
    // so `git clone` doesn't refuse the destination.
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }
    let output = std::process::Command::new("git")
        .arg("clone")
        .arg("--depth=1")
        .arg(url)
        .arg(&target)
        .output()
        .map_err(|_| ResolveError::GitNotInstalled)?;
    if !output.status.success() {
        return Err(ResolveError::GitCloneFailed {
            url: url.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_name_eq_file_url() {
        let s = parse_dep_line("Forui = \"file://../forui\"").unwrap();
        assert_eq!(s.name, "Forui");
        assert_eq!(s.url, "file://../forui");
    }

    #[test]
    fn parses_name_eq_https_url() {
        let s = parse_dep_line("Forui = \"https://github.com/forailang/forui\"").unwrap();
        assert_eq!(s.name, "Forui");
        assert_eq!(s.url, "https://github.com/forailang/forui");
    }

    #[test]
    fn skips_blank_and_comment_lines() {
        assert!(parse_dep_line("").is_none());
        assert!(parse_dep_line("# comment").is_none());
    }

    #[test]
    fn rejects_legacy_url_on_left_form() {
        // `"file://path" = "0.1.0"` was the old format. Returning None
        // surfaces it as "unknown dep" rather than silently mis-parsing.
        assert!(parse_dep_line("\"file://../forui\" = \"0.1.0\"").is_none());
        assert!(parse_dep_line("\"https://github.com/x/y\" = \"0.1.0\"").is_none());
    }

    #[test]
    fn computes_git_cache_path_strips_scheme_and_dotgit() {
        let root = PathBuf::from("/cache");
        assert_eq!(
            git_cache_path_for_url(&root, "https://github.com/forailang/forui"),
            PathBuf::from("/cache/git/github.com/forailang/forui")
        );
        assert_eq!(
            git_cache_path_for_url(&root, "https://github.com/forailang/forui.git"),
            PathBuf::from("/cache/git/github.com/forailang/forui")
        );
    }

    #[test]
    fn resolves_absolute_file_url_unchanged() {
        let p = resolve_dep_url("file:///tmp/somewhere", Path::new("/anywhere")).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/somewhere"));
    }

    #[test]
    fn resolves_relative_file_url_against_project_root() {
        let p = resolve_dep_url("file://../forui", Path::new("/home/me/app")).unwrap();
        assert_eq!(p, PathBuf::from("/home/me/app/../forui"));
    }

    #[test]
    fn resolves_https_url_short_circuits_on_cache_hit() {
        // Pre-populate a fake cache so resolve_dep_url never invokes git.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cache_dir = std::env::temp_dir().join(format!("fai-deptest-cache-{}", nonce));
        let url = "https://example.invalid/forailang/widget";
        let cache_path = git_cache_path_for_url(&cache_dir, url);
        std::fs::create_dir_all(&cache_path).unwrap();
        std::fs::write(
            cache_path.join("fai.toml"),
            "[project]\nname = \"Widget\"\n",
        )
        .unwrap();

        // SAFETY: this test sets a process-wide env var. Other tests
        // that read FAI_CACHE_DIR could be perturbed if run in
        // parallel, but currently no other test in the workspace
        // reads it.
        unsafe {
            std::env::set_var("FAI_CACHE_DIR", &cache_dir);
        }
        let resolved = resolve_dep_url(url, Path::new("/unused")).unwrap();
        unsafe {
            std::env::remove_var("FAI_CACHE_DIR");
        }
        assert_eq!(resolved, cache_path);

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn rejects_unknown_scheme() {
        let err = resolve_dep_url("ftp://example.com/x", Path::new("/")).unwrap_err();
        assert!(matches!(err, ResolveError::UnsupportedScheme(_)));
    }
}
