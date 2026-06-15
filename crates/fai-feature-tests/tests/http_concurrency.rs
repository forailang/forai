//! End-to-end HTTP server concurrency harness.
//!
//! Boots a real forai server via the `fai` binary in a subprocess, then drives
//! it over raw TCP. This is the verification foundation for the async I/O
//! boundary work (plan 101). The server exposes several routes that exercise
//! the unified driver loop's paths:
//!
//!   GET /       async handler, sleeps 500ms then 200 "ok"  (concurrency timing)
//!   GET /quick  async handler, sleeps 1ms then 200 "quick" (fast task churn)
//!   GET /sync   synchronous handler, 200 "sync"            (inline path)
//!   GET /boom   async handler, sleeps then throws          (FAILED -> 500)
//!   (any other path)                                       (404)
//!
//! The sleeping handlers occupy the runtime cooperatively, so concurrent
//! requests reveal whether the runtime serves them one-at-a-time or overlaps
//! them on the single scheduler thread.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

const SLOW_MS: u64 = 500;

fn server_source(port: u16) -> String {
    format!(
        "use std.http.server\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let router = server.router()\n\
         \x20 server.get(router, '/') do with req HttpRequest\n\
         \x20   sleep({slow})\n\
         \x20   server.text(200, 'ok')\n\
         \x20 end\n\
         \x20 server.get(router, '/quick') do with req HttpRequest\n\
         \x20   sleep(1)\n\
         \x20   server.text(200, 'quick')\n\
         \x20 end\n\
         \x20 server.get(router, '/sync') do with req HttpRequest\n\
         \x20   server.text(200, 'sync')\n\
         \x20 end\n\
         \x20 server.get(router, '/boom') do with req HttpRequest\n\
         \x20   sleep(1)\n\
         \x20   throw 7\n\
         \x20 end\n\
         \x20 server.listen(router, {port})\n\
         end\n",
        slow = SLOW_MS,
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

/// Issue one `GET <path>`, read the full response (server closes on `Connection:
/// close`), and return how long the round trip took plus the raw response.
fn timed_get(port: u16, path: &str) -> (Duration, String) {
    let start = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())
        .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    (start.elapsed(), response)
}

/// Fire `n` concurrent `GET <path>` requests; return each response and the
/// total wall-clock for the whole batch.
fn concurrent_gets(port: u16, path: &str, n: usize) -> (Duration, Vec<String>) {
    let start = Instant::now();
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let p = path.to_string();
            thread::spawn(move || timed_get(port, &p).1)
        })
        .collect();
    let responses: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    (start.elapsed(), responses)
}

#[test]
fn server_responds() {
    let server = boot_server();
    let (_t, response) = timed_get(server.port, "/");
    assert!(response.contains("200"), "expected 200, got:\n{response}");
    assert!(response.contains("ok"), "expected body 'ok', got:\n{response}");
}

#[test]
fn sync_handler_responds() {
    // A handler with no suspending call resolves inline (not spawned as a task).
    let server = boot_server();
    let (_t, response) = timed_get(server.port, "/sync");
    assert!(response.contains("200"), "expected 200, got:\n{response}");
    assert!(response.contains("sync"), "expected body 'sync', got:\n{response}");
}

#[test]
fn unmatched_path_404() {
    let server = boot_server();
    let (_t, response) = timed_get(server.port, "/does-not-exist");
    assert!(response.contains("404"), "expected 404, got:\n{response}");
}

#[test]
fn handler_error_500() {
    // An async handler that throws becomes a FAILED task; the driver loop must
    // answer 500 rather than writing its non-response result or hanging.
    let server = boot_server();
    let (_t, response) = timed_get(server.port, "/boom");
    assert!(response.contains("500"), "expected 500, got:\n{response}");
}

#[test]
fn concurrent_requests_overlap() {
    let server = boot_server();
    let (total, responses) = concurrent_gets(server.port, "/", 2);
    for r in &responses {
        assert!(r.contains("200"), "request failed:\n{r}");
    }
    // Served concurrently the pair finishes in ~one SLOW_MS; serially it takes
    // ~two. The 1.5x midpoint cleanly separates the regimes.
    let threshold = Duration::from_millis(SLOW_MS * 3 / 2);
    assert!(
        total < threshold,
        "expected concurrent serving (< {threshold:?}), took {total:?} — still serial?"
    );
}

#[test]
fn many_requests_overlap() {
    // Eight slow requests at once: concurrent serving finishes in ~one SLOW_MS,
    // serial would be ~eight. Proves the loop multiplexes well beyond two.
    let server = boot_server();
    let (total, responses) = concurrent_gets(server.port, "/", 8);
    assert_eq!(responses.len(), 8);
    for r in &responses {
        assert!(r.contains("200"), "request failed:\n{r}");
    }
    let threshold = Duration::from_millis(SLOW_MS * 5 / 2);
    assert!(
        total < threshold,
        "expected 8 requests to overlap (< {threshold:?}), took {total:?}"
    );
}

#[test]
fn sequential_soak_reuses_task_slots() {
    // Many sequential async requests churn spawn -> complete -> __fai_free_task.
    // If slot recycling were wrong the task table (capacity 4096) would not
    // exhaust at this count, but a wedge or corruption would surface as a failed
    // or missing response well before then. Also guards against a per-request
    // wedge in the driver loop.
    let server = boot_server();
    for i in 0..120 {
        let (_t, response) = timed_get(server.port, "/quick");
        assert!(
            response.contains("200") && response.contains("quick"),
            "request {i} failed:\n{response}"
        );
    }
}

/// Run a server with `--check-leaks` that exits after `max` requests, firing
/// exactly that many at `/quick`, and return the live-object count the leak
/// ledger reports at exit. Panics if the server doesn't exit or print a report.
fn live_objects_after(max: usize, path: &str) -> u64 {
    let port = free_port();
    let dir = workspace_root().join("target").join("tmp").join(format!(
        "http_leak_{}_{}",
        std::process::id(),
        port
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("server.fai");
    std::fs::write(&src, server_source(port)).unwrap();

    let mut child = Command::new(fai_binary())
        .arg("run")
        .arg("--check-leaks")
        .arg(&src)
        .env("FAI_HTTP_MAX_REQUESTS", max.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fai run --check-leaks");

    // Fire exactly `max` requests sequentially — no separate liveness probe,
    // since any accepted connection counts toward the cap. The first request
    // retries through server startup; each completes a full round trip, so the
    // server accepts exactly `max` connections and then drains and exits.
    let startup = Instant::now() + Duration::from_secs(20);
    for i in 0..max {
        loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(mut stream) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(15)))
                        .unwrap();
                    stream
                        .write_all(
                            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                                .as_bytes(),
                        )
                        .unwrap();
                    let mut resp = String::new();
                    stream.read_to_string(&mut resp).unwrap();
                    assert!(resp.contains("200"), "request {i} failed:\n{resp}");
                    break;
                }
                Err(_) if Instant::now() < startup => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    let _ = child.kill();
                    panic!("request {i} could not connect: {e}");
                }
            }
        }
    }

    // The process should now drain and exit on its own.
    let output = child.wait_with_output().expect("wait for server exit");
    let _ = std::fs::remove_dir_all(&dir);

    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find(|l| l.contains("[check-leaks] live heap:"))
        .unwrap_or_else(|| panic!("no [check-leaks] report in stderr:\n{stderr}"));
    // "[check-leaks] live heap: N objects, M bytes ..."
    line.split("live heap:")
        .nth(1)
        .and_then(|rest| rest.trim().split(' ').next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("could not parse live-object count from: {line}"))
}

#[test]
fn per_request_lifecycle_does_not_leak() {
    // A per-request leak (request/response graph, task slot, or closure not
    // reclaimed) would make the live-object count at exit grow with the number
    // of requests served. Serving 4x as many requests must leave the same live
    // set — only the server's persistent structures remain (R14).
    let few = live_objects_after(10, "/quick");
    let many = live_objects_after(40, "/quick");
    assert_eq!(
        few, many,
        "live objects grew with request count ({few} -> {many}): per-request leak"
    );
}

#[test]
fn mixed_sync_and_async_concurrent() {
    // A sync request must be served promptly even while a slow async handler is
    // in flight — the driver loop accepts and resolves it inline rather than
    // waiting behind the parked task.
    let server = boot_server();
    let port = server.port;
    let slow = thread::spawn(move || timed_get(port, "/").1);
    // Give the slow request a moment to be accepted and parked.
    thread::sleep(Duration::from_millis(50));
    let (sync_elapsed, sync_resp) = timed_get(port, "/sync");
    assert!(sync_resp.contains("sync"), "sync failed:\n{sync_resp}");
    assert!(
        sync_elapsed < Duration::from_millis(SLOW_MS),
        "sync request waited behind the async one: {sync_elapsed:?}"
    );
    assert!(slow.join().unwrap().contains("200"));
}
