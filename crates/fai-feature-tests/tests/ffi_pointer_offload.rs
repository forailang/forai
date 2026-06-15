//! Verifies that FFI offload (plan 101) handles pointer- and string-argument
//! externs, not just scalars — the shape real C libraries (e.g. SQLite) use.
//!
//! Builds a tiny C stub at test time (requires `cc`; the test is skipped if no
//! compiler is found) exposing:
//!   void* make_ctx(int n)       -- returns a heap pointer holding n
//!   int   slow_use_ctx(void* c) -- sleeps ~500ms, returns *c   (blocking, Ptr arg)
//!   int   str_len(const char* s)-- returns strlen(s)            (String arg)
//!
//! A forai server binds them via `extern ffistub` and a handler does
//! `let c = make_ctx(42); let n = slow_use_ctx(c)`. This exercises a Ptr return
//! tracked on the main thread, a Ptr arg resolved on the main thread and used
//! on a worker, and a String arg marshalled across the boundary — then checks
//! both correctness and that two concurrent blocking calls overlap.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

const STUB_C: &str = r#"
#include <unistd.h>
#include <string.h>
#include <stdlib.h>
void* make_ctx(int n) { int* p = malloc(sizeof(int)); *p = n; return p; }
int slow_use_ctx(void* c) { usleep(500000); return *(int*)c; }
int str_len(const char* s) { return (int)strlen(s); }
"#;

fn have_cc() -> bool {
    Command::new("cc")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

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
         use std.string\n\
         \n\
         extern ffistub\n\
         \x20 def make_ctx(n: Int) -> Ptr\n\
         \x20 def slow_use_ctx(ctx: Ptr) -> Int\n\
         \x20 def str_len(s: String) -> Int\n\
         end\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let router = server.router()\n\
         \x20 server.get(router, '/ctx') do with req HttpRequest\n\
         \x20   let c = make_ctx(42)\n\
         \x20   let n = slow_use_ctx(c)\n\
         \x20   server.text(200, toString(n))\n\
         \x20 end\n\
         \x20 server.get(router, '/strlen') do with req HttpRequest\n\
         \x20   let n = str_len('hello')\n\
         \x20   server.text(200, toString(n))\n\
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

/// Compile the C stub into `libffistub.so` in a temp dir and boot the forai
/// server with that dir on `LD_LIBRARY_PATH` so `extern ffistub` resolves.
fn boot_server() -> ServerProc {
    let port = free_port();
    let dir = workspace_root()
        .join("target")
        .join("tmp")
        .join(format!("ffi_ptr_{}_{}", std::process::id(), port));
    std::fs::create_dir_all(&dir).unwrap();
    let c_src = dir.join("stub.c");
    std::fs::write(&c_src, STUB_C).unwrap();
    let so = dir.join("libffistub.so");
    let cc = Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&so)
        .arg(&c_src)
        .status()
        .expect("run cc");
    assert!(cc.success(), "cc failed to build stub");

    let src = dir.join("server.fai");
    std::fs::write(&src, server_source(port)).unwrap();

    let ld_path = match std::env::var("LD_LIBRARY_PATH") {
        Ok(p) => format!("{}:{}", dir.display(), p),
        Err(_) => dir.display().to_string(),
    };
    let child = Command::new(fai_binary())
        .arg("run")
        .arg(&src)
        .env("LD_LIBRARY_PATH", ld_path)
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

fn get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    resp
}

#[test]
fn string_arg_offload_is_correct() {
    if !have_cc() {
        eprintln!("skipping: no cc");
        return;
    }
    let server = boot_server();
    let resp = get(server.port, "/strlen");
    assert!(resp.contains("200"), "got:\n{resp}");
    // strlen("hello") = 5 — proves the String arg survived marshalling across
    // the boundary worker.
    assert!(resp.contains('5'), "expected body 5, got:\n{resp}");
}

#[test]
fn pointer_arg_offload_is_correct() {
    if !have_cc() {
        eprintln!("skipping: no cc");
        return;
    }
    let server = boot_server();
    let resp = get(server.port, "/ctx");
    assert!(resp.contains("200"), "got:\n{resp}");
    // make_ctx(42) returns a Ptr (tracked on the main thread); slow_use_ctx
    // dereferences it on a worker and returns 42 — proves Ptr return + Ptr arg
    // round-trip through the boundary.
    assert!(resp.contains("42"), "expected body 42, got:\n{resp}");
}

#[test]
#[ignore = "red target: enable once pointer/string-arg externs offload (fai_ffi marshal/raw/unmarshal split)"]
fn concurrent_pointer_ffi_overlaps() {
    if !have_cc() {
        eprintln!("skipping: no cc");
        return;
    }
    let server = boot_server();
    let p = server.port;
    let start = Instant::now();
    let handles: Vec<_> = (0..2)
        .map(|_| thread::spawn(move || get(p, "/ctx")))
        .collect();
    for h in handles {
        assert!(h.join().unwrap().contains("42"));
    }
    let total = start.elapsed();
    // Both handlers make a ~500ms blocking pointer-arg C call. Offloaded they
    // overlap (~0.5s); serialized they take ~1s.
    assert!(
        total < Duration::from_millis(750),
        "expected pointer-arg FFI to overlap (< 750ms), took {total:?}"
    );
}
