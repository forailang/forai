//! Plan 103 U3: a slow HTTP client must cost a boundary waiter thread, never
//! the scheduler. Two shapes:
//!
//! - slow *sender*: opens a connection, drips half a request line, stalls.
//!   Pre-U3 the server read the request on the main thread with a 5s read
//!   timeout, so every other connection waited behind the stall.
//! - slow *reader*: sends a full request but never drains the response.
//!   Pre-U3 the response `write_all` ran on the main thread and blocked once
//!   the kernel send buffer filled.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn server_source(port: u16) -> String {
    // `/quick` answers immediately; `/big` answers with a ~4MiB body (larger
    // than a default kernel send buffer, so an unread response must park the
    // writer, not the scheduler).
    format!(
        "use std.http.server\n\
         use std.string\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let router = server.router()\n\
         \x20 server.get(router, '/quick') do with req HttpRequest\n\
         \x20   server.text(200, 'quick')\n\
         \x20 end\n\
         \x20 server.get(router, '/big') do with req HttpRequest\n\
         \x20   server.text(200, string.repeat('x', 4194304))\n\
         \x20 end\n\
         \x20 server.listen(router, {port})\n\
         end\n",
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

fn boot_server(port: u16) -> ServerProc {
    let dir = workspace_root().join("target").join("tmp").join(format!(
        "http_slow_client_{}_{}",
        std::process::id(),
        port
    ));
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

fn timed_quick_get(port: u16) -> (Duration, String) {
    let started = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .write_all(b"GET /quick HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read response");
    (started.elapsed(), resp)
}

/// A client that connects and sends only half a request line, then stalls.
/// Held open for the test's duration; pre-U3 this pinned the scheduler
/// thread inside the request read for up to the 5s read timeout.
fn open_stalled_sender(port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect stalled");
    stream.write_all(b"GET /qui").expect("write partial");
    stream
}

#[test]
fn stalled_request_sender_does_not_delay_other_connections() {
    let port = free_port();
    let server = boot_server(port);

    // Three stalled senders, held open across the whole check.
    let _stalled: Vec<TcpStream> = (0..3).map(|_| open_stalled_sender(server.port)).collect();
    // Give the server a beat to accept them and park their reads.
    thread::sleep(Duration::from_millis(300));

    let (elapsed, resp) = timed_quick_get(server.port);
    assert!(
        resp.contains("200") && resp.contains("quick"),
        "quick response wrong:\n{resp}"
    );
    // Pre-U3 each stalled sender blocked the scheduler until its 5s read
    // timeout; the quick request waited its turn (~5-15s). Off-thread reads
    // leave only driver-park latency (sub-second).
    assert!(
        elapsed < Duration::from_secs(2),
        "quick request stalled behind slow senders: {elapsed:?}"
    );
}

#[test]
fn unread_response_does_not_delay_other_connections() {
    let port = free_port();
    let server = boot_server(port);

    // Ask for the ~4MiB body but never read it: the server's write fills the
    // kernel send buffer and must park a waiter, not the scheduler.
    let mut big = TcpStream::connect(("127.0.0.1", server.port)).expect("connect big");
    big.write_all(b"GET /big HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    // Wait for the handler to run and the write to start (and jam).
    thread::sleep(Duration::from_millis(500));

    let (elapsed, resp) = timed_quick_get(server.port);
    assert!(
        resp.contains("200") && resp.contains("quick"),
        "quick response wrong:\n{resp}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "quick request stalled behind an unread response: {elapsed:?}"
    );
    // Cleanup: drain the big response so the server's waiter finishes.
    let mut sink = Vec::new();
    let _ = big.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = big.read_to_end(&mut sink);
}
