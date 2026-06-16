//! End-to-end verification that outbound RPC (`remoteCall` → `remote_begin`)
//! is offloaded to the blocking boundary instead of blocking the scheduler
//! thread (plan 101, U2/U6).
//!
//! Scaffolding: a local stub that answers `POST /fai/rpc` slowly, and a forai
//! server whose handler makes one `remoteCall` to it. Two concurrent handler
//! requests each make a ~500ms outbound call. If `remote_begin` blocks the
//! thread, the two calls run back-to-back (~1s); once it parks on the boundary,
//! they overlap (~0.5s).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

const RPC_DELAY_MS: u64 = 500;

/// Spawn a stub that answers any request to `…/fai/rpc` with
/// `{"ok":true,"value":"pong"}` after `delay`, one thread per connection so the
/// stub itself never serializes concurrent calls. Returns its port; the
/// listener thread lives until the test process exits.
fn spawn_rpc_stub(delay: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            thread::spawn(move || {
                // Best-effort read of the request (small; one read suffices).
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                thread::sleep(delay);
                let body = "{\"ok\":true,\"value\":\"pong\"}";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });
    port
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn server_source(port: u16, rpc_url: &str) -> String {
    format!(
        "use std.http.server\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let router = server.router()\n\
         \x20 server.get(router, '/rpc') do with req HttpRequest\n\
         \x20   let _ = remoteCall('{rpc_url}', 'ping', '[]', 'h')\n\
         \x20   server.text(200, 'done')\n\
         \x20 end\n\
         \x20 server.listen(router, {port})\n\
         end\n",
        rpc_url = rpc_url,
        port = port,
    )
}

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

fn boot_server(source: &str, port: u16) -> ServerProc {
    let dir = workspace_root().join("target").join("tmp").join(format!(
        "rpc_offload_{}_{}",
        std::process::id(),
        port
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("server.fai");
    std::fs::write(&src, source).unwrap();

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

fn get(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .write_all(b"GET /rpc HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    resp
}

#[test]
fn single_rpc_handler_responds() {
    let stub = spawn_rpc_stub(Duration::from_millis(RPC_DELAY_MS));
    let port = free_port();
    let server = boot_server(
        &server_source(port, &format!("http://127.0.0.1:{stub}")),
        port,
    );
    let resp = get(server.port);
    assert!(
        resp.contains("200") && resp.contains("done"),
        "got:\n{resp}"
    );
}

#[test]
fn concurrent_outbound_rpc_overlaps() {
    let stub = spawn_rpc_stub(Duration::from_millis(RPC_DELAY_MS));
    let port = free_port();
    let server = boot_server(
        &server_source(port, &format!("http://127.0.0.1:{stub}")),
        port,
    );
    let p = server.port;

    let start = Instant::now();
    let handles: Vec<_> = (0..2).map(|_| thread::spawn(move || get(p))).collect();
    for h in handles {
        assert!(h.join().unwrap().contains("200"));
    }
    let total = start.elapsed();

    // Both handlers make a ~RPC_DELAY_MS outbound call. Offloaded to the
    // boundary they overlap (~one delay); blocking the thread they serialize
    // (~two). The 1.5x midpoint separates the regimes.
    let threshold = Duration::from_millis(RPC_DELAY_MS * 3 / 2);
    assert!(
        total < threshold,
        "expected outbound RPC to overlap (< {threshold:?}), took {total:?} — remote_begin still blocking?"
    );
}
