use crate::templates;

pub(crate) fn cmd_new(args: &[String]) {
    let parsed = match parse_new_args(args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(1);
        }
    };

    let project_root = std::path::Path::new(&parsed.project_dir);

    if project_root.exists() {
        eprintln!("error: target already exists: {}", project_root.display());
        std::process::exit(1);
    }

    let project_name = project_root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    if let Some(tref_str) = &parsed.template {
        let tref = match templates::parse_template_ref(tref_str) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        };
        scaffold_from_template_ref(tref, project_root, &project_name);
        return;
    }

    inline_scaffold(project_root, &project_name);
}

struct NewArgs {
    project_dir: String,
    template: Option<String>,
}

fn parse_new_args(args: &[String]) -> Result<NewArgs, String> {
    let mut positional: Vec<String> = Vec::new();
    let mut template: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--template" => {
                i += 1;
                if i >= args.len() {
                    return Err("error: --template requires a value".to_string());
                }
                template = Some(args[i].clone());
            }
            "--yes" | "-y" => {
                // Reserved for future confirmation prompts (network mode).
                // Currently a no-op for the local-template path.
            }
            arg if arg.starts_with("--") => {
                return Err(format!("error: unknown flag: {}", arg));
            }
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    if positional.is_empty() {
        return Err("Usage: forai new <project-dir> [--template <ref>]".to_string());
    }
    if positional.len() > 1 {
        return Err(format!(
            "error: expected one project directory, got {}",
            positional.len()
        ));
    }
    Ok(NewArgs {
        project_dir: positional.into_iter().next().unwrap(),
        template,
    })
}

fn scaffold_from_template_ref(
    tref: templates::TemplateRef,
    project_root: &std::path::Path,
    project_name: &str,
) {
    match tref {
        templates::TemplateRef::Local(path) => {
            let opts = templates::ScaffoldOptions {
                template_root: &path,
                target_dir: project_root,
                project_name,
            };
            if let Err(e) = templates::scaffold_from_local(&opts) {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
            overlay_meta_files(project_root, project_name);
            println!("scaffolded {} from {}", project_name, path.display());
        }
        templates::TemplateRef::Github {
            owner,
            repo,
            git_ref,
        } => {
            scaffold_from_github(
                &owner,
                &repo,
                git_ref.as_deref(),
                project_root,
                project_name,
            );
        }
        templates::TemplateRef::Url { .. } => {
            eprintln!("error: arbitrary URL templates are not yet supported");
            eprintln!("note: use the GitHub shorthand `<owner>/<repo>[#ref]`");
            std::process::exit(1);
        }
    }
}

#[cfg(feature = "http-client")]
fn scaffold_from_github(
    owner: &str,
    repo: &str,
    git_ref: Option<&str>,
    project_root: &std::path::Path,
    project_name: &str,
) {
    let ref_label = git_ref.unwrap_or("HEAD");
    println!(
        "fetching https://github.com/{}/{} ({})",
        owner, repo, ref_label
    );
    let template_root = match templates::fetch_github_template(owner, repo, git_ref) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    let opts = templates::ScaffoldOptions {
        template_root: &template_root,
        target_dir: project_root,
        project_name,
    };
    let res = templates::scaffold_from_local(&opts);
    let _ = std::fs::remove_dir_all(&template_root);
    if let Err(e) = res {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
    overlay_meta_files(project_root, project_name);
    println!(
        "scaffolded {} from {}/{} ({})",
        project_name, owner, repo, ref_label
    );
}

#[cfg(not(feature = "http-client"))]
fn scaffold_from_github(
    _owner: &str,
    _repo: &str,
    _git_ref: Option<&str>,
    _project_root: &std::path::Path,
    _project_name: &str,
) {
    eprintln!("error: this fai build was compiled without the `http-client` feature");
    eprintln!("note: rebuild with `--features http-client` to use network templates");
    std::process::exit(1);
}

fn inline_scaffold(project_root: &std::path::Path, project_name: &str) {
    let src_dir = project_root.join("src");
    if let Err(e) = std::fs::create_dir_all(&src_dir) {
        eprintln!("error creating directory: {}", e);
        std::process::exit(1);
    }

    let project_files: Vec<(std::path::PathBuf, String)> = vec![
        (src_dir.join("main.fai"), scaffold_main(project_name)),
        (
            project_root.join("fai.toml"),
            scaffold_fai_toml(project_name),
        ),
        (
            project_root.join("README.md"),
            scaffold_readme(project_name),
        ),
    ];

    for (path, content) in &project_files {
        if let Err(e) = std::fs::write(path, content) {
            eprintln!("error writing {}: {}", path.display(), e);
            std::process::exit(1);
        }
    }

    overlay_meta_files(project_root, project_name);

    println!("created project '{}'", project_name);
}

/// Write language-level metadata files (`CLAUDE.md`, `AGENTS.md`,
/// `language.md`, `.mcp.json`, `.codex/config.toml`) into a project
/// directory. These belong with the language tooling, not with any
/// individual template — `fai new` overlays them onto every new
/// project regardless of template source.
///
/// `AGENTS.md` and `CLAUDE.md` are special: when the template ships
/// its own copy, the scaffold's language-level guidance is written
/// first and the template's content is appended below a separator.
/// This keeps language-level rules (doc comments, testing) visible
/// while preserving template-specific guidance the user picked.
///
/// All other files use last-write-wins semantics: a file the template
/// already shipped is left alone; anything missing is filled in.
pub(crate) fn overlay_meta_files(dir: &std::path::Path, project_name: &str) {
    let codex_dir = dir.join(".codex");
    if !codex_dir.exists() {
        let _ = std::fs::create_dir_all(&codex_dir);
    }

    // Append-on-collision: language scaffold + template-shipped content.
    let merging: Vec<(std::path::PathBuf, String)> = vec![
        (dir.join("CLAUDE.md"), scaffold_claude_md(project_name)),
        (dir.join("AGENTS.md"), scaffold_agents_md()),
    ];
    for (path, scaffold) in &merging {
        write_with_template_append(path, scaffold);
    }

    // Fill-only-if-missing: language reference + tool configs.
    let fill_only: Vec<(std::path::PathBuf, String)> = vec![
        (dir.join("language.md"), scaffold_language_md()),
        (dir.join(".mcp.json"), scaffold_mcp_json()),
        (dir.join(".codex/config.toml"), scaffold_codex_config()),
    ];
    for (path, content) in &fill_only {
        if path.exists() {
            continue;
        }
        if let Err(e) = std::fs::write(path, content) {
            eprintln!("warning: could not write {}: {}", path.display(), e);
        }
    }
}

/// Write `scaffold` to `path`. If the template already shipped a file
/// at this path, append its content below a separator so both stay
/// visible. The scaffold goes first because language-level rules
/// (e.g. doc-comment requirement) are universal and should be the
/// first thing an agent reads.
fn write_with_template_append(path: &std::path::Path, scaffold: &str) {
    let template_content = if path.exists() {
        std::fs::read_to_string(path).ok()
    } else {
        None
    };
    let combined = match template_content {
        Some(t) if !t.trim().is_empty() => {
            format!(
                "{}\n---\n\n# Project-specific guidance\n\n{}",
                scaffold.trim_end(),
                t.trim_start()
            )
        }
        _ => scaffold.to_string(),
    };
    if let Err(e) = std::fs::write(path, combined) {
        eprintln!("warning: could not write {}: {}", path.display(), e);
    }
}

pub(crate) fn scaffold_main(project_name: &str) -> String {
    format!(
        r#"# {name} entry point

def main
    @return Void
do
  print('hello from {name}')
end
"#,
        name = project_name
    )
}

pub(crate) fn scaffold_fai_toml(project_name: &str) -> String {
    format!(
        r#"[project]
name = "{name}"
version = "0.1.0"
source_root = "src"

[dependencies]
"#,
        name = project_name
    )
}

pub(crate) fn scaffold_readme(project_name: &str) -> String {
    format!(
        r#"# {name}

A forai project.

## Commands

```bash
fai run        # fmt → check → test → run
fai check      # fmt → check
fai test       # fmt → check → test
fai fmt        # format source files
fai build      # fmt → check → test → build (.wasm)
```
"#,
        name = project_name
    )
}

pub(crate) fn scaffold_language_md() -> String {
    include_str!("../templates/language.md").to_string()
}

pub(crate) fn scaffold_claude_md(project_name: &str) -> String {
    include_str!("../templates/CLAUDE.md").replace("__FAI_PROJECT_NAME__", project_name)
}

pub(crate) fn scaffold_agents_md() -> String {
    include_str!("../templates/AGENTS.md").to_string()
}

fn scaffold_mcp_json() -> String {
    r#"{
  "mcpServers": {
    "fai": {
      "command": "fai",
      "args": ["mcp"]
    }
  }
}
"#
    .to_string()
}

fn scaffold_codex_config() -> String {
    r#"[mcp_servers.fai]
command = "fai"
args = ["mcp"]
enabled = true
startup_timeout_sec = 10
tool_timeout_sec = 120
"#
    .to_string()
}
