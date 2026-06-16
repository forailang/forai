//! End-to-end verification that blocking FFI calls are offloaded to the
//! blocking boundary instead of blocking the scheduler thread (plan 101,
//! U7–U9). This is the SQLite/DB-in-handler use case in miniature.
//!
//! Stub: a forai server whose handler makes one blocking C call —
//! `usleep` from libc, bound via `extern c` — for ~500ms before responding.
//! No custom C toolchain needed; libc is always present. If the FFI call blocks
//! the thread, two concurrent handler requests serialize (~1s); once extern
//! calls are async-colored and offloaded, they overlap (~0.5s).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

const SLEEP_US: u64 = 500_000; // 500ms

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn server_source(port: u16) -> String {
    format!(
        "use std.http.server\n\
         \n\
         extern c\n\
         \x20 def usleep(usec: Int) -> Int\n\
         end\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let router = server.router()\n\
         \x20 server.get(router, '/sleep') do with req HttpRequest\n\
         \x20   let _ = usleep({us})\n\
         \x20   server.text(200, 'slept')\n\
         \x20 end\n\
         \x20 server.listen(router, {port})\n\
         end\n",
        us = SLEEP_US,
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
        "ffi_offload_{}_{}",
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

fn get(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .write_all(b"GET /sleep HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    resp
}

#[test]
fn single_ffi_handler_responds() {
    let port = free_port();
    let server = boot_server(port);
    let resp = get(server.port);
    assert!(
        resp.contains("200") && resp.contains("slept"),
        "got:\n{resp}"
    );
}

#[test]
fn concurrent_blocking_ffi_overlaps() {
    let port = free_port();
    let server = boot_server(port);
    let p = server.port;

    let start = Instant::now();
    let handles: Vec<_> = (0..2).map(|_| thread::spawn(move || get(p))).collect();
    for h in handles {
        assert!(h.join().unwrap().contains("200"));
    }
    let total = start.elapsed();

    // Both handlers make a ~500ms blocking C call. Offloaded to the boundary
    // they overlap (~one sleep); blocking the thread they serialize (~two).
    let threshold = Duration::from_millis((SLEEP_US / 1000) * 3 / 2);
    assert!(
        total < threshold,
        "expected blocking FFI to overlap (< {threshold:?}), took {total:?} — still blocking the loop?"
    );
}
