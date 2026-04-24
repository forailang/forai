fn main() {
    fai_cli::cli_main();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    /// Path to the compiled `fai` binary.
    fn fai() -> PathBuf {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest.parent().unwrap().parent().unwrap();
        let bin = workspace.join("target").join("debug").join("fai");
        assert!(
            bin.exists(),
            "fai binary not found at {}; run `cargo build` first",
            bin.display()
        );
        bin
    }

    /// Write a temp .fai file and return its path. Caller is responsible for cleanup.
    fn write_fai(tag: &str, src: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fai_cli_fai_test_{}", tag));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prog.fai");
        std::fs::write(&path, src).unwrap();
        path
    }

    const HELLO: &str = concat!(
        "def main\n",
        "    @return Void\n",
        "do\n",
        "  print('hello from fai')\n",
        "end\n",
    );

    const RETURN_INT: &str = concat!("def main\n", "    @return Int\n", "do\n", "  42\n", "end\n",);

    const TYPE_ERROR: &str = concat!(
        "def main\n",
        "    @return Int\n",
        "do\n",
        "  'this is a string not an int'\n",
        "end\n",
    );

    // ── fai --help ───────────────────────────────────────────────────────

    #[test]
    fn test_help_flag() {
        let out = Command::new(fai()).arg("--help").output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Commands:"),
            "expected usage in stderr, got: {}",
            stderr
        );
    }

    #[test]
    fn test_help_subcommand() {
        let out = Command::new(fai()).arg("help").output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("Commands:"));
    }

    // ── fai run ──────────────────────────────────────────────────────────

    #[test]
    fn test_run_prints_output() {
        let path = write_fai("run_hello", HELLO);
        let out = Command::new(fai())
            .args(["run", path.to_str().unwrap()])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "fai run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("hello from fai"),
            "expected output, got: {}",
            stdout
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_run_returns_int_value() {
        let path = write_fai("run_int", RETURN_INT);
        let out = Command::new(fai())
            .args(["run", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("42"),
            "expected 42 in output, got: {}",
            stdout
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_run_shorthand_without_subcommand() {
        // `fai file.fai` is shorthand for `fai run file.fai`
        let path = write_fai("run_shorthand", HELLO);
        let out = Command::new(fai())
            .arg(path.to_str().unwrap())
            .output()
            .unwrap();
        assert!(out.status.success(), "shorthand run failed");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_run_missing_file_exits_nonzero() {
        let out = Command::new(fai())
            .args(["run", "/nonexistent/missing.fai"])
            .output()
            .unwrap();
        assert!(!out.status.success());
    }

    // ── fai check ────────────────────────────────────────────────────────

    #[test]
    fn test_check_valid_program_exits_zero() {
        let path = write_fai("check_ok", HELLO);
        let out = Command::new(fai())
            .args(["check", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "check failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("ok"), "expected 'ok', got: {}", stdout);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_check_type_error_exits_nonzero() {
        let path = write_fai("check_err", TYPE_ERROR);
        let out = Command::new(fai())
            .args(["check", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "expected check to fail on type error"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.is_empty(), "expected error message in stderr");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ── fai fmt ──────────────────────────────────────────────────────────

    #[test]
    fn test_fmt_already_formatted_says_so() {
        let path = write_fai("fmt_ok", "let x = 42\n");
        let out = Command::new(fai())
            .args(["fmt", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("already formatted"), "got: {}", stdout);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_fmt_check_mode_passes_for_formatted_file() {
        let path = write_fai("fmt_check", "let x = 42\n");
        let out = Command::new(fai())
            .args(["fmt", path.to_str().unwrap(), "--check"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("ok"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_fmt_rewrites_unformatted_file() {
        // File without trailing newline — formatter adds it
        let path = write_fai("fmt_rewrite", "let x = 42");
        let out = Command::new(fai())
            .args(["fmt", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("formatted"), "got: {}", stdout);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "let x = 42\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ── fai new ──────────────────────────────────────────────────────────

    #[test]
    fn test_new_creates_project_structure() {
        let base = std::env::temp_dir().join("fai_cli_new_test");
        let project = base.join("myapp");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&base).unwrap();

        let out = Command::new(fai())
            .args(["new", project.to_str().unwrap()])
            .output()
            .unwrap();

        assert!(
            out.status.success(),
            "fai new failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(project.join("src").join("main.fai").exists());
        assert!(project.join("fai.toml").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    // ── unknown command ──────────────────────────────────────────────────

    #[test]
    fn test_unknown_command_exits_nonzero() {
        let out = Command::new(fai()).arg("notacommand").output().unwrap();
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("unknown command") || stderr.contains("Usage"));
    }

    #[test]
    fn test_no_args_exits_nonzero() {
        let out = Command::new(fai()).output().unwrap();
        assert!(!out.status.success());
    }
}
