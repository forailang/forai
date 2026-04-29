//! Template fetching and scaffolding for `fai new`.
//!
//! See `plans/fai-new-templates.md` for the design. Three reference
//! shapes are accepted:
//!
//! - `./forai-blank` — local directory; no fetch, just copy.
//! - `BrianBal/my-template` — GitHub `owner/repo` shorthand, default
//!   branch unless `#tag-or-sha` is appended.
//! - `https://gitlab.com/foo/bar` — any host, full URL, optional `#ref`.

use std::path::{Path, PathBuf};

/// Top-level directory names that the scaffolder skips when copying
/// a template into a fresh project. Each of these is either VCS state
/// or a build artifact dir — never something a template intends to
/// ship.
const IGNORED_TOP_LEVEL: &[&str] = &[".git", "build", "target", "node_modules"];

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum TemplateRef {
    Local(PathBuf),
    Github {
        owner: String,
        repo: String,
        git_ref: Option<String>,
    },
    Url {
        url: String,
        git_ref: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    Ambiguous(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Empty => write!(f, "template reference is empty"),
            ParseError::Ambiguous(s) => write!(
                f,
                "could not interpret '{}' as a local path, GitHub shorthand, or full URL",
                s
            ),
        }
    }
}

/// Parse a template reference string into a [`TemplateRef`].
///
/// Local paths must be prefixed with `./`, `../`, `/`, or `~/` so they
/// can't be confused with `owner/repo` shorthand.
pub fn parse_template_ref(s: &str) -> Result<TemplateRef, ParseError> {
    if s.is_empty() {
        return Err(ParseError::Empty);
    }

    if s.starts_with("./") || s.starts_with("../") || s.starts_with('/') || s.starts_with("~/") {
        return Ok(TemplateRef::Local(PathBuf::from(s)));
    }

    let (base, git_ref) = match s.find('#') {
        Some(i) => {
            let r = &s[i + 1..];
            (&s[..i], if r.is_empty() { None } else { Some(r.to_string()) })
        }
        None => (s, None),
    };

    if base.starts_with("http://") || base.starts_with("https://") {
        return Ok(TemplateRef::Url {
            url: base.to_string(),
            git_ref,
        });
    }

    let segs: Vec<&str> = base.split('/').collect();
    if segs.len() == 2 && !segs[0].is_empty() && !segs[1].is_empty() {
        return Ok(TemplateRef::Github {
            owner: segs[0].to_string(),
            repo: segs[1].to_string(),
            git_ref,
        });
    }

    Err(ParseError::Ambiguous(s.to_string()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum RewriteError {
    NoProjectSection,
    NoNameField,
}

impl std::fmt::Display for RewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RewriteError::NoProjectSection => write!(f, "fai.toml has no [project] section"),
            RewriteError::NoNameField => {
                write!(f, "fai.toml [project] section has no `name` field")
            }
        }
    }
}

/// Rewrite the `name = "..."` field inside `fai.toml`'s `[project]`
/// section. Preserves comments, whitespace, and other sections
/// verbatim.
///
/// Only the first `name` line in `[project]` is touched; sibling
/// subtables like `[project.web]` are left alone (they don't normally
/// declare `name`, but we'd ignore it if they did).
pub fn rewrite_project_name(content: &str, new_name: &str) -> Result<String, RewriteError> {
    let mut out = String::with_capacity(content.len() + new_name.len());
    let mut in_project = false;
    let mut saw_project = false;
    let mut replaced = false;

    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_project = trimmed == "[project]";
            if in_project {
                saw_project = true;
            }
        } else if in_project && !replaced {
            if let Some(rewritten) = try_rewrite_name_line(line, new_name) {
                out.push_str(&rewritten);
                replaced = true;
                continue;
            }
        }
        out.push_str(line);
    }

    if !saw_project {
        return Err(RewriteError::NoProjectSection);
    }
    if !replaced {
        return Err(RewriteError::NoNameField);
    }
    Ok(out)
}

/// If `line` is shaped like `<indent>name = "..."` (or with single
/// quotes), produce a replacement using `new_name`. Returns `None`
/// when the line is anything else (comments, other keys, blank lines).
fn try_rewrite_name_line(line: &str, new_name: &str) -> Option<String> {
    let trimmed_start = line.trim_start();
    let indent = &line[..line.len() - trimmed_start.len()];

    let (body, eol) = match trimmed_start.find('\n') {
        Some(i) => {
            let body_end = if i > 0 && trimmed_start.as_bytes()[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            (&trimmed_start[..body_end], &trimmed_start[body_end..])
        }
        None => (trimmed_start, ""),
    };

    let after_name = body.strip_prefix("name")?;
    let after_name = after_name.trim_start();
    let after_eq = after_name.strip_prefix('=')?.trim_start();

    let quote = after_eq.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let after_open = &after_eq[1..];
    let close = after_open.find(quote)?;
    let trailing = &after_open[close + 1..];

    Some(format!(
        "{}name = \"{}\"{}{}",
        indent, new_name, trailing, eol
    ))
}

#[derive(Debug)]
pub enum CopyError {
    SourceMissing(PathBuf),
    SourceNotADir(PathBuf),
    TargetNotEmpty(PathBuf),
    Io(std::io::Error),
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CopyError::SourceMissing(p) => write!(f, "template source not found: {}", p.display()),
            CopyError::SourceNotADir(p) => {
                write!(f, "template source is not a directory: {}", p.display())
            }
            CopyError::TargetNotEmpty(p) => write!(
                f,
                "target directory is not empty: {} (refuse to overwrite)",
                p.display()
            ),
            CopyError::Io(e) => write!(f, "{}", e),
        }
    }
}

impl From<std::io::Error> for CopyError {
    fn from(e: std::io::Error) -> Self {
        CopyError::Io(e)
    }
}

/// Copy a template directory to `dst`, skipping VCS state and known
/// build-artifact directories at the top level. The target must be
/// empty (or absent — we'll create it).
///
/// This is the core of `fai new`'s local-path mode and the post-fetch
/// step of the network mode.
pub fn copy_template_tree(src: &Path, dst: &Path) -> Result<(), CopyError> {
    if !src.exists() {
        return Err(CopyError::SourceMissing(src.to_path_buf()));
    }
    if !src.is_dir() {
        return Err(CopyError::SourceNotADir(src.to_path_buf()));
    }

    if dst.exists() {
        let mut iter = std::fs::read_dir(dst)?;
        if iter.next().is_some() {
            return Err(CopyError::TargetNotEmpty(dst.to_path_buf()));
        }
    } else {
        std::fs::create_dir_all(dst)?;
    }

    copy_recursive(src, dst, true)
}

fn copy_recursive(src: &Path, dst: &Path, is_top: bool) -> Result<(), CopyError> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if is_top {
            if let Some(s) = name.to_str() {
                if IGNORED_TOP_LEVEL.contains(&s) {
                    continue;
                }
            }
        }
        let child_src = entry.path();
        let child_dst = dst.join(&name);
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_recursive(&child_src, &child_dst, false)?;
        } else if ft.is_file() {
            std::fs::copy(&child_src, &child_dst)?;
        }
        // symlinks intentionally skipped in v1 — templates shouldn't ship them
    }
    Ok(())
}

#[derive(Debug)]
pub enum ScaffoldError {
    Copy(CopyError),
    Rewrite(RewriteError),
    TemplateMissingFaiToml(PathBuf),
    Io(std::io::Error),
}

impl std::fmt::Display for ScaffoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScaffoldError::Copy(e) => write!(f, "{}", e),
            ScaffoldError::Rewrite(e) => write!(f, "fai.toml: {}", e),
            ScaffoldError::TemplateMissingFaiToml(p) => {
                write!(f, "template has no fai.toml at: {}", p.display())
            }
            ScaffoldError::Io(e) => write!(f, "{}", e),
        }
    }
}

impl From<CopyError> for ScaffoldError {
    fn from(e: CopyError) -> Self {
        ScaffoldError::Copy(e)
    }
}

impl From<RewriteError> for ScaffoldError {
    fn from(e: RewriteError) -> Self {
        ScaffoldError::Rewrite(e)
    }
}

impl From<std::io::Error> for ScaffoldError {
    fn from(e: std::io::Error) -> Self {
        ScaffoldError::Io(e)
    }
}

pub struct ScaffoldOptions<'a> {
    pub template_root: &'a Path,
    pub target_dir: &'a Path,
    pub project_name: &'a str,
}

/// Scaffold a new project at `target_dir` from a local template
/// directory. Copies the tree (skipping VCS/build dirs at the top
/// level) and rewrites `[project].name` in `fai.toml` to the new
/// project name.
///
/// Network-fetched templates land here too once the tarball has been
/// extracted to a temp dir.
pub fn scaffold_from_local(opts: &ScaffoldOptions) -> Result<(), ScaffoldError> {
    copy_template_tree(opts.template_root, opts.target_dir)?;
    let toml_path = opts.target_dir.join("fai.toml");
    if !toml_path.exists() {
        return Err(ScaffoldError::TemplateMissingFaiToml(toml_path));
    }
    let content = std::fs::read_to_string(&toml_path)?;
    let rewritten = rewrite_project_name(&content, opts.project_name)?;
    std::fs::write(&toml_path, rewritten)?;
    Ok(())
}

/// Build the codeload tarball URL for a GitHub repo.
///
/// `git_ref` is `None` for the default branch (resolved as `HEAD`),
/// or `Some(...)` for a tag, branch, or full SHA. Refs are passed
/// through unescaped — GitHub accepts the same encoding it uses
/// in URLs (`/`, `.`, etc. are fine in tag names).
pub fn github_tarball_url(owner: &str, repo: &str, git_ref: Option<&str>) -> String {
    let r = git_ref.unwrap_or("HEAD");
    format!(
        "https://codeload.github.com/{}/{}/tar.gz/{}",
        owner, repo, r
    )
}

#[derive(Debug)]
pub enum FetchError {
    Network(String),
    HttpStatus(u16),
    Extract(std::io::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Network(s) => write!(f, "network error: {}", s),
            FetchError::HttpStatus(c) => write!(f, "HTTP {}", c),
            FetchError::Extract(e) => write!(f, "tarball extraction failed: {}", e),
            FetchError::Io(e) => write!(f, "{}", e),
        }
    }
}

impl From<std::io::Error> for FetchError {
    fn from(e: std::io::Error) -> Self {
        FetchError::Io(e)
    }
}

/// GET a URL and return the response body bytes. Limited to 50 MB to
/// keep a runaway response from eating memory on a typo'd template URL.
#[cfg(feature = "http-client")]
pub fn fetch_url(url: &str) -> Result<Vec<u8>, FetchError> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .new_agent();

    let mut resp = agent
        .get(url)
        .call()
        .map_err(|e| FetchError::Network(format!("{}", e)))?;

    let status = resp.status().as_u16();
    if status != 200 {
        return Err(FetchError::HttpStatus(status));
    }

    let bytes = resp
        .body_mut()
        .with_config()
        .limit(50 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| FetchError::Network(format!("{}", e)))?;

    Ok(bytes)
}

/// Allocate a fresh, empty temp directory under the system temp root.
/// Caller is responsible for cleanup.
pub fn make_temp_dir(tag: &str) -> std::io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "fai-{}-{}-{}",
        tag,
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Fetch a GitHub template tarball, extract it, flatten the
/// codeload-style `<repo>-<ref>/` wrapper directory, and return the
/// path to the extracted template root. Caller scaffolds from that
/// path and removes it when done.
#[cfg(feature = "http-client")]
pub fn fetch_github_template(
    owner: &str,
    repo: &str,
    git_ref: Option<&str>,
) -> Result<PathBuf, FetchError> {
    let url = github_tarball_url(owner, repo, git_ref);
    let bytes = fetch_url(&url)?;
    let tmp = make_temp_dir("template")?;
    extract_targz(&bytes, &tmp).map_err(FetchError::Extract)?;
    flatten_single_top_dir(&tmp).map_err(FetchError::Extract)?;
    Ok(tmp)
}

/// Decompress + extract a tar.gz archive into `dest`. The destination
/// is created if missing and must be empty when extraction starts.
#[cfg(feature = "http-client")]
pub fn extract_targz(bytes: &[u8], dest: &Path) -> Result<(), std::io::Error> {
    use std::io::Cursor;
    if !dest.exists() {
        std::fs::create_dir_all(dest)?;
    }
    let gz = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(gz);
    archive.unpack(dest)?;
    Ok(())
}

/// GitHub tarballs always wrap their contents in a single top-level
/// directory like `repo-main/`. After extraction we want the project
/// at the root of `dest`, not nested. If `dest` contains exactly one
/// directory and nothing else, lift its contents up one level.
///
/// No-op when `dest` already contains files at the root (or no entries).
pub fn flatten_single_top_dir(dest: &Path) -> Result<(), std::io::Error> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dest)? {
        entries.push(entry?);
    }
    if entries.len() != 1 {
        return Ok(());
    }
    let only = &entries[0];
    if !only.file_type()?.is_dir() {
        return Ok(());
    }
    let inner = only.path();
    for child in std::fs::read_dir(&inner)? {
        let child = child?;
        let new_path = dest.join(child.file_name());
        std::fs::rename(child.path(), new_path)?;
    }
    std::fs::remove_dir(&inner)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fai_templates_test_{}", tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_local_relative_path() {
        let r = parse_template_ref("./forai-blank").unwrap();
        assert_eq!(r, TemplateRef::Local(PathBuf::from("./forai-blank")));
    }

    #[test]
    fn parses_local_parent_path() {
        let r = parse_template_ref("../foo").unwrap();
        assert_eq!(r, TemplateRef::Local(PathBuf::from("../foo")));
    }

    #[test]
    fn parses_local_absolute_path() {
        let r = parse_template_ref("/tmp/foo").unwrap();
        assert_eq!(r, TemplateRef::Local(PathBuf::from("/tmp/foo")));
    }

    #[test]
    fn parses_local_home_path() {
        let r = parse_template_ref("~/projects/foo").unwrap();
        assert_eq!(r, TemplateRef::Local(PathBuf::from("~/projects/foo")));
    }

    #[test]
    fn parses_github_shorthand() {
        let r = parse_template_ref("BrianBal/my-template").unwrap();
        assert_eq!(
            r,
            TemplateRef::Github {
                owner: "BrianBal".to_string(),
                repo: "my-template".to_string(),
                git_ref: None,
            }
        );
    }

    #[test]
    fn parses_github_shorthand_with_tag_ref() {
        let r = parse_template_ref("BrianBal/my-template#v0.3").unwrap();
        assert_eq!(
            r,
            TemplateRef::Github {
                owner: "BrianBal".to_string(),
                repo: "my-template".to_string(),
                git_ref: Some("v0.3".to_string()),
            }
        );
    }

    #[test]
    fn parses_github_shorthand_with_branch_ref() {
        let r = parse_template_ref("forai-lang/forai-blank#main").unwrap();
        assert_eq!(
            r,
            TemplateRef::Github {
                owner: "forai-lang".to_string(),
                repo: "forai-blank".to_string(),
                git_ref: Some("main".to_string()),
            }
        );
    }

    #[test]
    fn parses_https_url() {
        let r = parse_template_ref("https://github.com/foo/bar").unwrap();
        assert_eq!(
            r,
            TemplateRef::Url {
                url: "https://github.com/foo/bar".to_string(),
                git_ref: None,
            }
        );
    }

    #[test]
    fn parses_https_url_with_ref() {
        let r = parse_template_ref("https://gitlab.com/foo/bar#main").unwrap();
        assert_eq!(
            r,
            TemplateRef::Url {
                url: "https://gitlab.com/foo/bar".to_string(),
                git_ref: Some("main".to_string()),
            }
        );
    }

    #[test]
    fn parses_http_url() {
        let r = parse_template_ref("http://localhost/foo/bar").unwrap();
        assert_eq!(
            r,
            TemplateRef::Url {
                url: "http://localhost/foo/bar".to_string(),
                git_ref: None,
            }
        );
    }

    #[test]
    fn empty_hash_suffix_is_treated_as_no_ref() {
        // `foo/bar#` is degenerate; treat it the same as `foo/bar`.
        let r = parse_template_ref("BrianBal/my-template#").unwrap();
        assert_eq!(
            r,
            TemplateRef::Github {
                owner: "BrianBal".to_string(),
                repo: "my-template".to_string(),
                git_ref: None,
            }
        );
    }

    #[test]
    fn rejects_empty_string() {
        assert_eq!(parse_template_ref(""), Err(ParseError::Empty));
    }

    #[test]
    fn rejects_three_segment_path_without_prefix() {
        // `foo/bar/baz` could be a path or a deeply-nested ref. Without
        // a `./` or `/` prefix we refuse rather than guess.
        let r = parse_template_ref("foo/bar/baz");
        assert!(matches!(r, Err(ParseError::Ambiguous(_))));
    }

    #[test]
    fn rejects_single_word() {
        let r = parse_template_ref("standalone");
        assert!(matches!(r, Err(ParseError::Ambiguous(_))));
    }

    #[test]
    fn rejects_owner_with_empty_repo() {
        let r = parse_template_ref("BrianBal/");
        assert!(matches!(r, Err(ParseError::Ambiguous(_))));
    }

    #[test]
    fn rejects_empty_owner() {
        let r = parse_template_ref("/repo");
        // This is a Local path (starts with `/`) — by design.
        assert_eq!(r, Ok(TemplateRef::Local(PathBuf::from("/repo"))));
    }

    // ── rewrite_project_name ─────────────────────────────────────────

    #[test]
    fn rewrites_simple_project_name() {
        let input = "[project]\nname = \"Old\"\nversion = \"0.1.0\"\n";
        let out = rewrite_project_name(input, "MyApp").unwrap();
        assert_eq!(out, "[project]\nname = \"MyApp\"\nversion = \"0.1.0\"\n");
    }

    #[test]
    fn preserves_other_sections() {
        let input = "\
[project]
name = \"Old\"
version = \"0.1.0\"

[dependencies]
\"file:///foo\" = \"1.0\"
";
        let out = rewrite_project_name(input, "New").unwrap();
        assert!(out.contains("[dependencies]"));
        assert!(out.contains("\"file:///foo\""));
        assert!(out.contains("name = \"New\""));
        assert!(!out.contains("\"Old\""));
    }

    #[test]
    fn preserves_comments_and_indentation() {
        let input = "\
# Top comment.
[project]
# project name
name = \"Old\"
version = \"0.1.0\"
";
        let out = rewrite_project_name(input, "New").unwrap();
        assert!(out.contains("# Top comment."));
        assert!(out.contains("# project name"));
        assert!(out.contains("name = \"New\""));
    }

    #[test]
    fn does_not_touch_name_in_other_sections() {
        let input = "\
[project]
name = \"Old\"

[some.other]
name = \"DontTouch\"
";
        let out = rewrite_project_name(input, "New").unwrap();
        assert!(out.contains("[some.other]"));
        assert!(out.contains("DontTouch"));
        assert_eq!(out.matches("name = \"New\"").count(), 1);
    }

    #[test]
    fn does_not_touch_name_in_project_subsection() {
        // The `name` field on `[project.web]` (if it ever appeared)
        // shouldn't be confused with the project's own name.
        let input = "\
[project]
name = \"Outer\"

[project.web]
name = \"InnerKeep\"
target = \"wasm-html\"
";
        let out = rewrite_project_name(input, "New").unwrap();
        assert!(out.contains("[project.web]"));
        assert!(out.contains("InnerKeep"));
        assert!(out.contains("name = \"New\""));
        assert!(!out.contains("Outer"));
    }

    #[test]
    fn handles_single_quoted_value() {
        let input = "[project]\nname = 'Old'\n";
        let out = rewrite_project_name(input, "New").unwrap();
        // Always emits double quotes — that's the canonical TOML form
        // produced by `fai new` even when the source used single quotes.
        assert!(out.contains("name = \"New\""));
        assert!(!out.contains('\''));
    }

    #[test]
    fn errors_when_no_project_section() {
        let input = "[other]\nname = \"x\"\n";
        assert_eq!(
            rewrite_project_name(input, "New"),
            Err(RewriteError::NoProjectSection)
        );
    }

    #[test]
    fn errors_when_project_section_has_no_name() {
        let input = "[project]\nversion = \"1.0\"\n";
        assert_eq!(
            rewrite_project_name(input, "New"),
            Err(RewriteError::NoNameField)
        );
    }

    #[test]
    fn errors_when_name_only_in_subsection() {
        // `[project.web]` declaring a `name` field should not satisfy
        // the requirement of `[project]` having one.
        let input = "\
[project]
version = \"1.0\"

[project.web]
name = \"WebName\"
";
        assert_eq!(
            rewrite_project_name(input, "New"),
            Err(RewriteError::NoNameField)
        );
    }

    #[test]
    fn handles_no_trailing_newline() {
        let input = "[project]\nname = \"Old\"";
        let out = rewrite_project_name(input, "New").unwrap();
        assert_eq!(out, "[project]\nname = \"New\"");
    }

    #[test]
    fn preserves_inline_comment_after_name() {
        let input = "[project]\nname = \"Old\" # the project name\n";
        let out = rewrite_project_name(input, "New").unwrap();
        assert!(out.contains("name = \"New\""));
        assert!(out.contains("# the project name"));
    }

    // ── copy_template_tree ───────────────────────────────────────────

    #[test]
    fn copies_simple_files() {
        let base = temp_dir("copy_simple");
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("foo")).unwrap();
        std::fs::write(src.join("a.txt"), "alpha").unwrap();
        std::fs::write(src.join("foo/b.txt"), "bravo").unwrap();

        copy_template_tree(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
        assert_eq!(
            std::fs::read_to_string(dst.join("foo/b.txt")).unwrap(),
            "bravo"
        );
    }

    #[test]
    fn skips_dot_git_at_top_level() {
        let base = temp_dir("copy_skip_git");
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join(".git/objects")).unwrap();
        std::fs::write(src.join(".git/HEAD"), "ref").unwrap();
        std::fs::write(src.join("README.md"), "# hi").unwrap();

        copy_template_tree(&src, &dst).unwrap();

        assert!(!dst.join(".git").exists());
        assert!(dst.join("README.md").exists());
    }

    #[test]
    fn skips_build_at_top_level() {
        let base = temp_dir("copy_skip_build");
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("build/web")).unwrap();
        std::fs::write(src.join("build/web/main.wasm"), "wasm").unwrap();
        std::fs::write(src.join("fai.toml"), "[project]").unwrap();

        copy_template_tree(&src, &dst).unwrap();

        assert!(!dst.join("build").exists());
        assert!(dst.join("fai.toml").exists());
    }

    #[test]
    fn skips_target_at_top_level() {
        let base = temp_dir("copy_skip_target");
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("target/debug")).unwrap();
        std::fs::write(src.join("target/debug/x"), "x").unwrap();
        std::fs::write(src.join("Cargo.toml"), "[package]").unwrap();

        copy_template_tree(&src, &dst).unwrap();

        assert!(!dst.join("target").exists());
        assert!(dst.join("Cargo.toml").exists());
    }

    #[test]
    fn does_not_skip_nested_named_dirs() {
        // A directory called 'build' nested deep in the template is
        // user content, not the top-level build-artifact dir. Keep it.
        let base = temp_dir("copy_nested_named");
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("src/build")).unwrap();
        std::fs::write(src.join("src/build/keep.txt"), "keep").unwrap();

        copy_template_tree(&src, &dst).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("src/build/keep.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn creates_target_directory_if_missing() {
        let base = temp_dir("copy_creates_target");
        let src = base.join("src");
        let dst = base.join("nested/dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a"), "alpha").unwrap();

        copy_template_tree(&src, &dst).unwrap();

        assert!(dst.join("a").exists());
    }

    #[test]
    fn refuses_when_target_is_non_empty() {
        let base = temp_dir("copy_refuses");
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("existing"), "yo").unwrap();

        let r = copy_template_tree(&src, &dst);
        assert!(matches!(r, Err(CopyError::TargetNotEmpty(_))));
    }

    #[test]
    fn errors_when_source_missing() {
        let base = temp_dir("copy_src_missing");
        let src = base.join("nope");
        let dst = base.join("dst");
        let r = copy_template_tree(&src, &dst);
        assert!(matches!(r, Err(CopyError::SourceMissing(_))));
    }

    #[test]
    fn errors_when_source_is_a_file() {
        let base = temp_dir("copy_src_is_file");
        let src = base.join("foo.txt");
        let dst = base.join("dst");
        std::fs::write(&src, "x").unwrap();
        let r = copy_template_tree(&src, &dst);
        assert!(matches!(r, Err(CopyError::SourceNotADir(_))));
    }

    // ── scaffold_from_local ──────────────────────────────────────────

    #[test]
    fn scaffolds_from_local_template() {
        let base = temp_dir("scaffold_local");
        let tpl = base.join("tpl");
        let dst = base.join("dst");

        std::fs::create_dir_all(tpl.join("src")).unwrap();
        std::fs::write(
            tpl.join("fai.toml"),
            "[project]\nname = \"Old\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(tpl.join("src/main.fai"), "def main\n").unwrap();

        scaffold_from_local(&ScaffoldOptions {
            template_root: &tpl,
            target_dir: &dst,
            project_name: "MyApp",
        })
        .unwrap();

        let toml = std::fs::read_to_string(dst.join("fai.toml")).unwrap();
        assert!(toml.contains("name = \"MyApp\""));
        assert!(!toml.contains("Old"));
        assert!(dst.join("src/main.fai").exists());
    }

    #[test]
    fn scaffold_skips_template_dot_git() {
        let base = temp_dir("scaffold_drops_git");
        let tpl = base.join("tpl");
        let dst = base.join("dst");

        std::fs::create_dir_all(tpl.join(".git")).unwrap();
        std::fs::write(tpl.join(".git/HEAD"), "ref").unwrap();
        std::fs::write(
            tpl.join("fai.toml"),
            "[project]\nname = \"Old\"\n",
        )
        .unwrap();

        scaffold_from_local(&ScaffoldOptions {
            template_root: &tpl,
            target_dir: &dst,
            project_name: "Fresh",
        })
        .unwrap();

        assert!(!dst.join(".git").exists());
    }

    #[test]
    fn scaffold_errors_when_template_lacks_fai_toml() {
        let base = temp_dir("scaffold_no_toml");
        let tpl = base.join("tpl");
        let dst = base.join("dst");
        std::fs::create_dir_all(&tpl).unwrap();
        std::fs::write(tpl.join("README.md"), "# hi").unwrap();

        let r = scaffold_from_local(&ScaffoldOptions {
            template_root: &tpl,
            target_dir: &dst,
            project_name: "MyApp",
        });
        assert!(matches!(r, Err(ScaffoldError::TemplateMissingFaiToml(_))));
    }

    #[test]
    fn scaffold_propagates_rewrite_error() {
        let base = temp_dir("scaffold_rewrite_err");
        let tpl = base.join("tpl");
        let dst = base.join("dst");
        std::fs::create_dir_all(&tpl).unwrap();
        std::fs::write(tpl.join("fai.toml"), "[other]\n").unwrap();

        let r = scaffold_from_local(&ScaffoldOptions {
            template_root: &tpl,
            target_dir: &dst,
            project_name: "MyApp",
        });
        assert!(matches!(
            r,
            Err(ScaffoldError::Rewrite(RewriteError::NoProjectSection))
        ));
    }

    // ── github_tarball_url ───────────────────────────────────────────

    #[test]
    fn tarball_url_default_branch_is_head() {
        assert_eq!(
            github_tarball_url("forailang", "starter-template", None),
            "https://codeload.github.com/forailang/starter-template/tar.gz/HEAD"
        );
    }

    #[test]
    fn tarball_url_with_tag() {
        assert_eq!(
            github_tarball_url("foo", "bar", Some("v1.0")),
            "https://codeload.github.com/foo/bar/tar.gz/v1.0"
        );
    }

    #[test]
    fn tarball_url_with_branch() {
        assert_eq!(
            github_tarball_url("foo", "bar", Some("main")),
            "https://codeload.github.com/foo/bar/tar.gz/main"
        );
    }

    #[test]
    fn tarball_url_with_sha() {
        assert_eq!(
            github_tarball_url("foo", "bar", Some("a3b91c2deadbeef")),
            "https://codeload.github.com/foo/bar/tar.gz/a3b91c2deadbeef"
        );
    }

    // ── extract_targz ────────────────────────────────────────────────

    #[cfg(feature = "http-client")]
    fn build_targz_fixture(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut compressed = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::default());
            let mut tar_b = tar::Builder::new(gz);
            for (path, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar_b.append(&header, *data).unwrap();
            }
            tar_b.into_inner().unwrap().finish().unwrap();
        }
        compressed
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn extracts_targz_into_directory() {
        let bytes = build_targz_fixture(&[
            ("repo-main/fai.toml", b"[project]\nname = \"X\"\n"),
            ("repo-main/src/main.fai", b"def main\n"),
        ]);
        let base = temp_dir("extract_targz");
        let dest = base.join("out");
        extract_targz(&bytes, &dest).unwrap();
        assert!(dest.join("repo-main/fai.toml").exists());
        assert!(dest.join("repo-main/src/main.fai").exists());
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn extracts_targz_creates_dest_if_missing() {
        let bytes = build_targz_fixture(&[("repo-main/a.txt", b"alpha")]);
        let base = temp_dir("extract_targz_create");
        let dest = base.join("nested/out");
        extract_targz(&bytes, &dest).unwrap();
        assert!(dest.join("repo-main/a.txt").exists());
    }

    // ── flatten_single_top_dir ───────────────────────────────────────

    #[test]
    fn flatten_lifts_contents_up_one_level() {
        let base = temp_dir("flatten_lifts");
        let dir = base.join("scratch");
        std::fs::create_dir_all(dir.join("repo-main/src/foo")).unwrap();
        std::fs::write(dir.join("repo-main/fai.toml"), "x").unwrap();
        std::fs::write(dir.join("repo-main/src/foo/a.fai"), "y").unwrap();

        flatten_single_top_dir(&dir).unwrap();

        assert!(dir.join("fai.toml").exists());
        assert!(dir.join("src/foo/a.fai").exists());
        assert!(!dir.join("repo-main").exists());
    }

    #[test]
    fn flatten_noop_when_multiple_entries() {
        let base = temp_dir("flatten_multi");
        let dir = base.join("scratch");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("b"), "x").unwrap();

        flatten_single_top_dir(&dir).unwrap();

        assert!(dir.join("a").exists());
        assert!(dir.join("b").exists());
    }

    #[test]
    fn flatten_noop_when_only_top_level_is_a_file() {
        let base = temp_dir("flatten_only_file");
        let dir = base.join("scratch");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("solo.txt"), "x").unwrap();

        flatten_single_top_dir(&dir).unwrap();

        assert!(dir.join("solo.txt").exists());
    }

    #[test]
    fn flatten_noop_on_empty_dir() {
        let base = temp_dir("flatten_empty");
        let dir = base.join("scratch");
        std::fs::create_dir_all(&dir).unwrap();

        flatten_single_top_dir(&dir).unwrap();

        // Still empty; just shouldn't error.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn scaffold_refuses_non_empty_target() {
        let base = temp_dir("scaffold_refuses");
        let tpl = base.join("tpl");
        let dst = base.join("dst");
        std::fs::create_dir_all(&tpl).unwrap();
        std::fs::write(tpl.join("fai.toml"), "[project]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("conflict"), "no").unwrap();

        let r = scaffold_from_local(&ScaffoldOptions {
            template_root: &tpl,
            target_dir: &dst,
            project_name: "MyApp",
        });
        assert!(matches!(
            r,
            Err(ScaffoldError::Copy(CopyError::TargetNotEmpty(_)))
        ));
    }
}
