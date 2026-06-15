//! End-to-end HTTP server concurrency harness.
//!
//! Boots a real forai server via the `fai` binary in a subprocess, then drives
//! it over raw TCP. This is the verification foundation for the async I/O
//! boundary work (plan 101, U2–U4): the server's handler sleeps before
//! responding, so concurrent requests reveal whether the runtime serves them
//! one-at-a-time (today) or overlaps them on the single scheduler thread (after
//! the driver-loop rewrite).
//!
//! `server_responds` proves the harness and the current server work and runs in
//! the normal suite. `concurrent_requests_overlap` is the red target for U3/U4
//! and is `#[ignore]`d until the driver loop lands — run it with
//! `cargo test -p fai-feature-tests --test http_concurrency -- --ignored`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

const HANDLER_SLEEP_MS: u64 = 500;

/// A forai server whose `/` handler sleeps `HANDLER_SLEEP_MS` then returns
/// `200 ok`. The sleep makes the handler async (suspending), so the request
/// occupies the runtime for the sleep duration — the signal the concurrency
/// test measures.
fn server_source(port: u16) -> String {
    format!(
        "use std.http.server\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let router = server.router()\n\
         \x20 server.get(router, '/') do with req HttpRequest\n\
         \x20   sleep({sleep})\n\
         \x20   server.text(200, 'ok')\n\
         \x20 end\n\
         \x20 server.listen(router, {port})\n\
         end\n",
        sleep = HANDLER_SLEEP_MS,
        port = port,
    )
}

/// A child `fai run` server process plus its temp source dir. Killed and
/// cleaned up on drop so a panicking assertion never leaks a live server.
struct ServerProc {
    child: Child,
    dir: std::path::PathBuf,
    port: u16,
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Pick a port the OS just confirmed is free, then release it. Small race
/// window before the server rebinds it; acceptable for a local test.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Write the server source to a unique temp dir, spawn `fai run`, and wait
/// until it accepts connections. Panics with captured output on failure.
fn boot_server() -> ServerProc {
    let port = free_port();
    let dir = workspace_root()
        .join("target")
        .join("tmp")
        .join(format!("http_concurrency_{}_{}", std::process::id(), port));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("server.fai");
    std::fs::write(&src, server_source(port)).unwrap();

    let child = Command::new(fai_binary())
        .arg("run")
        .arg(&src)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fai run");

    let mut proc = ServerProc { child, dir, port };

    // Poll until the server is listening, or the process exits / times out.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return proc;
        }
        if let Ok(Some(status)) = proc.child.try_wait() {
            let mut err = String::new();
            if let Some(mut s) = proc.child.stderr.take() {
                let _ = s.read_to_string(&mut err);
            }
            panic!("fai server exited early ({status}); stderr:\n{err}");
        }
        if Instant::now() >= deadline {
            panic!("fai server did not start listening within 20s");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Issue one `GET /`, read the full response (server closes on `Connection:
/// close`), and return how long the round trip took plus the raw response.
fn timed_get(port: u16) -> (Duration, String) {
    let start = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    (start.elapsed(), response)
}

#[test]
fn server_responds() {
    let server = boot_server();
    let (_elapsed, response) = timed_get(server.port);
    assert!(
        response.contains("200"),
        "expected 200 status, got:\n{response}"
    );
    assert!(
        response.contains("ok"),
        "expected body 'ok', got:\n{response}"
    );
}

#[test]
fn concurrent_requests_overlap() {
    let server = boot_server();
    let port = server.port;

    let start = Instant::now();
    let handles: Vec<_> = (0..2)
        .map(|_| thread::spawn(move || timed_get(port)))
        .collect();
    for h in handles {
        let (_elapsed, response) = h.join().unwrap();
        assert!(response.contains("200"), "request failed:\n{response}");
    }
    let total = start.elapsed();

    // Two handlers that each sleep HANDLER_SLEEP_MS. Served concurrently the
    // pair finishes in ~one sleep; served serially it takes ~two. The midpoint
    // (1.5x) cleanly separates the two regimes.
    let threshold = Duration::from_millis(HANDLER_SLEEP_MS * 3 / 2);
    assert!(
        total < threshold,
        "expected concurrent serving (< {threshold:?}), took {total:?} — still serial?"
    );
}
