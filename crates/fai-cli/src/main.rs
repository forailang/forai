fn main() {
    fai_cli::cli_main();
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    fn forai() -> PathBuf {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest.parent().unwrap().parent().unwrap();
        let bin = workspace.join("target").join("debug").join("forai");
        assert!(
            bin.exists(),
            "forai binary not found at {}; run `cargo build` first",
            bin.display()
        );
        bin
    }

    fn write_fai(tag: &str, src: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forai_cli_test_{}", tag));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prog.fai");
        std::fs::write(&path, src).unwrap();
        path
    }

    const HELLO: &str = concat!(
        "def main\n",
        "    @return Void\n",
        "do\n",
        "  print('hello from forai')\n",
        "end\n",
    );

    const RETURN_INT: &str = concat!("def main\n", "    @return Int\n", "do\n", "  99\n", "end\n",);

    const TYPE_ERROR: &str = concat!(
        "def main\n",
        "    @return Int\n",
        "do\n",
        "  'not an int'\n",
        "end\n",
    );

    // ── forai --help ─────────────────────────────────────────────────────

    #[test]
    fn test_help_flag() {
        let out = Command::new(forai()).arg("--help").output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Commands:"),
            "expected usage in stderr, got: {}",
            stderr
        );
    }

    // ── forai run ────────────────────────────────────────────────────────

    #[test]
    fn test_run_prints_output() {
        let path = write_fai("run_hello", HELLO);
        let out = Command::new(forai())
            .args(["run", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "forai run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("hello from forai"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_run_returns_int_value() {
        let path = write_fai("run_int", RETURN_INT);
        let out = Command::new(forai())
            .args(["run", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("99"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_run_shorthand() {
        let path = write_fai("run_shorthand", HELLO);
        let out = Command::new(forai())
            .arg(path.to_str().unwrap())
            .output()
            .unwrap();
        assert!(out.status.success());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_run_missing_file_exits_nonzero() {
        let out = Command::new(forai())
            .args(["run", "/nonexistent/missing.fai"])
            .output()
            .unwrap();
        assert!(!out.status.success());
    }

    // ── forai check ──────────────────────────────────────────────────────

    #[test]
    fn test_check_valid_program_exits_zero() {
        let path = write_fai("check_ok", HELLO);
        let out = Command::new(forai())
            .args(["check", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("ok"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_check_type_error_exits_nonzero() {
        let path = write_fai("check_err", TYPE_ERROR);
        let out = Command::new(forai())
            .args(["check", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(!out.status.success());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ── forai fmt ────────────────────────────────────────────────────────

    #[test]
    fn test_fmt_already_formatted() {
        let path = write_fai("fmt_ok", "let x = 42\n");
        let out = Command::new(forai())
            .args(["fmt", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("already formatted"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_fmt_check_mode_passes() {
        let path = write_fai("fmt_check", "let x = 42\n");
        let out = Command::new(forai())
            .args(["fmt", path.to_str().unwrap(), "--check"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // ── forai new ────────────────────────────────────────────────────────

    #[test]
    fn test_new_creates_project() {
        let base = std::env::temp_dir().join("forai_new_test");
        let project = base.join("myproject");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&base).unwrap();

        let out = Command::new(forai())
            .args(["new", project.to_str().unwrap()])
            .output()
            .unwrap();

        assert!(out.status.success());
        assert!(project.join("src").join("main.fai").exists());
        assert!(project.join("fai.toml").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    // ── edge cases ───────────────────────────────────────────────────────

    #[test]
    fn test_unknown_command_exits_nonzero() {
        let out = Command::new(forai()).arg("notacommand").output().unwrap();
        assert!(!out.status.success());
    }

    #[test]
    fn test_no_args_exits_nonzero() {
        let out = Command::new(forai()).output().unwrap();
        assert!(!out.status.success());
    }
}
