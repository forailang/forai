//! Plan 103 U1/U2: long-lived waits must not starve the blocking boundary,
//! and `process.write` must never block the scheduler thread.
//!
//! These are end-to-end `fai run` tests. Timing assertions live *inside* the
//! forai program (via `time.now()`) where compile overhead can't skew them;
//! the Rust side asserts on the printed verdict plus a coarse wall-clock
//! ceiling for the hang cases.

use std::process::Command;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

fn run_program(name: &str, source: &str, timeout: Duration) -> (String, String, Duration) {
    let dir = workspace_root()
        .join("target")
        .join("tmp")
        .join(format!("boundary_starvation_{}_{}", std::process::id(), name));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("main.fai");
    std::fs::write(&src, source).unwrap();

    let started = Instant::now();
    let mut child = Command::new(fai_binary())
        .arg("run")
        .arg(&src)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn fai run");
    let deadline = started + timeout;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(&dir);
                    panic!("`fai run {name}` did not finish within {timeout:?} — scheduler hung?");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    let elapsed = started.elapsed();
    let out = child.wait_with_output().expect("output");
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        elapsed,
    )
}

/// R1: more concurrent `process.run` waits than the bounded pool has threads
/// (8) all overlap. Parked on pool slots they'd run in waves (>= 2s for ten
/// 1s sleeps); on waiter threads the batch takes ~1s.
#[test]
fn more_process_runs_than_pool_slots_overlap() {
    // Note the tuple-destructured `all` — the engine-native form. The
    // single-binding array form (`let results = all(...)`) falls back to the
    // legacy facade, which can't lower `process.run` (pre-existing gap,
    // resurfaces in the U8 fixture audit).
    let source = "use std.process\n\
        use std.time\n\
        \n\
        def main\n\
        \x20   @return Void\n\
        do\n\
        \x20 let t0 = time.now()\n\
        \x20 let a, b, c, d, e, f, g, h, i, j = all(s(), s(), s(), s(), s(), s(), s(), s(), s(), s())\n\
        \x20 let elapsed = time.now() - t0\n\
        \x20 let total = a + b + c + d + e + f + g + h + i + j\n\
        \x20 let verdict = verdictFor(elapsed, total)\n\
        \x20 print(verdict)\n\
        end\n\
        \n\
        private:\n\
        \n\
        # One second-long external wait.\n\
        def s\n\
        \x20   @return Int\n\
        do\n\
        \x20 let _ = process.run('sleep 1', '.', '{}', 5000, 65536)\n\
        \x20 1\n\
        end\n\
        \n\
        # Report whether ten 1s waits overlapped.\n\
        def verdictFor\n\
        \x20   @param elapsed Float\n\
        \x20   @param count Int\n\
        \x20   @return String\n\
        do\n\
        \x20 if elapsed < 1800.0 and count == 10\n\
        \x20   'overlapped'\n\
        \x20 else\n\
        \x20   'serialized'\n\
        \x20 end\n\
        end\n\
        \n\
        test s\n\
        \x20 it 'completes'\n\
        \x20   s()\n\
        \x20 end\n\
        end\n\
        \n\
        test verdictFor\n\
        \x20 it 'classifies'\n\
        \x20   assert.equals(verdictFor(100.0, 10), 'overlapped')\n\
        \x20   assert.equals(verdictFor(3000.0, 10), 'serialized')\n\
        \x20 end\n\
        end\n";
    let (stdout, stderr, _) = run_program("overlap", source, Duration::from_secs(60));
    assert!(
        stdout.contains("overlapped"),
        "ten concurrent process.run should overlap on waiter threads.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// R2: `process.write` past a full stdin pipe (child never reads) must not
/// block the scheduler: the main task keeps running, and `process.stop`
/// unsticks the blocked writer by killing the child. If the write ran on the
/// scheduler thread the program would stall until the child died (~30s).
#[test]
fn full_pipe_process_write_keeps_runtime_responsive() {
    let source = "use std.process\n\
        use std.json\n\
        use std.string\n\
        \n\
        def main\n\
        \x20   @return Void\n\
        do\n\
        \x20 let startRaw = process.start('sleep 30', '.', '{}', 30000)\n\
        \x20 let started Dictionary = json.parse(startRaw)\n\
        \x20 let sessionId = getString(started, 'sessionId')!\n\
        \x20 nowait writeBig(sessionId)\n\
        \x20 sleep(200)\n\
        \x20 print('responsive')\n\
        \x20 let _ = process.stop(sessionId)\n\
        \x20 print('stopped')\n\
        end\n\
        \n\
        private:\n\
        \n\
        # Write more than a pipe buffer to a child that never reads stdin.\n\
        def writeBig\n\
        \x20   @param sessionId String\n\
        \x20   @return Void\n\
        do\n\
        \x20 let chunk = string.repeat('x', 262144)\n\
        \x20 let _ = process.write(sessionId, chunk)\n\
        end\n\
        \n\
        test writeBig\n\
        \x20 it 'tolerates a missing session'\n\
        \x20   writeBig('no-such-session')\n\
        \x20 end\n\
        end\n";
    let (stdout, stderr, elapsed) = run_program("fullpipe", source, Duration::from_secs(45));
    assert!(
        stdout.contains("responsive") && stdout.contains("stopped"),
        "main task should keep running past a stuck write.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Well under the child's 30s lifetime: proves the stuck write neither
    // blocked the scheduler nor outlived process.stop's unstick.
    assert!(
        elapsed < Duration::from_secs(20),
        "run took {elapsed:?} — write blocked the runtime until the child died?"
    );
}
