//! Regression pin for the ISSUES.md "two `rpc_server` targets in one
//! project share a single RPC surface" bug, fixed by plan 100's
//! reachability-scoped RPC surface collection.
//!
//! The tracked project fixture has two `rpc_server = true` targets over a
//! shared `src` tree. `api`'s main reaches a `remote def` (`getSecret`)
//! returning a type the `tasks` main never imports. Before plan 100, the
//! `tasks` build inherited every `remote def` in the source root and died
//! with `Unknown type 'ApiSecret'` in its generated dispatch. Now each
//! target's surface is scoped to its own reachable graph, so both build.
//!
//! Lives as a Rust integration test rather than a walk-discovered project
//! fixture: the discovered lane drives `fai run`, which requires
//! `--project` for multi-target projects; the assertion here is about
//! `fai build` of all targets.

use fai_feature_tests::{fai_binary, project_fixtures_root};
use std::fs;
use std::process::Command;

#[test]
fn two_rpc_server_targets_build_with_scoped_surfaces() {
    let project = project_fixtures_root().join("two_rpc_servers");
    assert!(
        project.join("fai.toml").is_file(),
        "fixture missing at {}",
        project.display()
    );
    // Clean build output so schema assertions can't pass on stale files.
    let _ = fs::remove_dir_all(project.join("build"));

    let out = Command::new(fai_binary())
        .arg("build")
        .current_dir(&project)
        .output()
        .expect("spawn fai build");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && !combined.contains("[fail]"),
        "multi-target build failed:\n{}",
        combined
    );
    assert!(
        !combined.contains("Unknown type"),
        "tasks target inherited api's RPC surface:\n{}",
        combined
    );

    // api reaches getSecret → its build has an RPC schema mentioning it.
    let api_schema = project.join("build/api/schema.json");
    assert!(
        api_schema.is_file(),
        "api target should emit schema.json:\n{}",
        combined
    );
    let schema = fs::read_to_string(&api_schema).expect("read api schema");
    assert!(
        schema.contains("getSecret"),
        "api schema should expose getSecret, got:\n{}",
        schema
    );

    // tasks reaches no remote defs → no RPC schema (and in particular
    // no inherited getSecret dispatch).
    let tasks_schema = project.join("build/tasks/schema.json");
    if tasks_schema.is_file() {
        let schema = fs::read_to_string(&tasks_schema).expect("read tasks schema");
        assert!(
            !schema.contains("getSecret"),
            "tasks schema must not inherit api's remote defs, got:\n{}",
            schema
        );
    }

    // Leave no build output in the tracked fixture.
    let _ = fs::remove_dir_all(project.join("build"));
}
