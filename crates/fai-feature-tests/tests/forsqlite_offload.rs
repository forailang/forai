//! End-to-end verification of the headline use case (plan 101 U10): a real
//! SQLite query inside an HTTP handler, via the forsqlite library, that does not
//! block the server. forsqlite's `query` calls `sqlite3_step` (a Ptr-arg extern)
//! in binding/assignment position, so the blocking step offloads to the
//! boundary; a CPU-heavy recursive-CTE count therefore overlaps across two
//! concurrent requests instead of serializing.
//!
//! Requires `cc`/libsqlite3 (present in CI image) and builds a temp project that
//! depends on the local forsqlite checkout. Skipped if forsqlite isn't found.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

/// Counts to ~2M via a recursive CTE — heavy enough that one query takes a
/// clearly measurable time, so concurrent overlap is distinguishable from
/// serial execution.
const SLOW_QUERY: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 2000000) SELECT count(*) AS n FROM c";

fn forsqlite_dir() -> Option<std::path::PathBuf> {
    let d = workspace_root().parent()?.join("forsqlite");
    if d.join("fai.toml").exists() {
        Some(d)
    } else {
        None
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn project_main(port: u16) -> String {
    format!(
        "use std.http.server\n\
         use std.events\n\
         use std.string\n\
         use {{ open, query }} from Forsqlite\n\
         \n\
         var beforeValue = ''\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let shared = open(':memory:')\n\
         \x20 let eventDb = open(':memory:')\n\
         \x20 let router = server.router()\n\
         \x20 let _before = events.on('http:beforeRequest') do with e Event\n\
         \x20   let req HttpRequest = from_dict(e.data)\n\
         \x20   if req.path == '/event'\n\
         \x20     let rows = query(eventDb, '{q}')\n\
         \x20     beforeValue = toString(getInt(rows[0], 'n')!)\n\
         \x20   end\n\
         \x20 end\n\
         \x20 server.get(router, '/q') do with req HttpRequest\n\
         \x20   let db = open(':memory:')\n\
         \x20   let rows = query(db, '{q}')\n\
         \x20   server.text(200, toString(getInt(rows[0], 'n')!))\n\
         \x20 end\n\
         \x20 server.get(router, '/event') do with req HttpRequest\n\
         \x20   server.text(200, beforeValue)\n\
         \x20 end\n\
         \x20 server.get(router, '/shared') do with req HttpRequest\n\
         \x20   let rows = query(shared, '{q}')\n\
         \x20   server.text(200, toString(getInt(rows[0], 'n')!))\n\
         \x20 end\n\
         \x20 server.listen(router, {port})\n\
         end\n",
        q = SLOW_QUERY,
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

fn boot_server(forsqlite: &std::path::Path) -> ServerProc {
    let port = free_port();
    let dir = workspace_root().join("target").join("tmp").join(format!(
        "forsqlite_{}_{}",
        std::process::id(),
        port
    ));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("fai.toml"),
        format!(
            "[project]\nname = \"SqliteOffloadTest\"\nsource_root = \"src\"\n\n[dependencies]\nForsqlite = \"file://{}\"\n",
            forsqlite.display()
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src").join("main.fai"), project_main(port)).unwrap();

    let child = Command::new(fai_binary())
        .arg("run")
        .arg("src/main.fai")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fai run");
    let mut proc = ServerProc { child, dir, port };

    let deadline = Instant::now() + Duration::from_secs(40);
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
            panic!("fai server did not start listening within 40s");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn timed_get(port: u16) -> (Duration, String) {
    let start = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .write_all(b"GET /q HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    (start.elapsed(), resp)
}

#[test]
fn sqlite_query_in_handler_is_correct() {
    let Some(forsqlite) = forsqlite_dir() else {
        eprintln!("skipping: forsqlite checkout not found");
        return;
    };
    let server = boot_server(&forsqlite);
    let (_t, resp) = timed_get(server.port);
    assert!(resp.contains("200"), "got:\n{resp}");
    // count(*) over the recursive CTE = 2_000_000.
    assert!(
        resp.contains("2000000"),
        "expected count 2000000, got:\n{resp}"
    );
}

#[test]
fn sqlite_query_in_before_request_event_completes_before_handler() {
    let Some(forsqlite) = forsqlite_dir() else {
        eprintln!("skipping: forsqlite checkout not found");
        return;
    };
    let server = boot_server(&forsqlite);
    let resp = get_path(server.port, "/event");
    assert!(resp.contains("200"), "got:\n{resp}");
    assert!(
        resp.contains("2000000"),
        "expected beforeRequest SQLite query result before handler, got:\n{resp}"
    );
}

fn get_path(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    resp
}

#[test]
fn concurrent_queries_on_shared_connection_stay_correct() {
    // Two concurrent requests hit ONE connection (opened in main, captured by
    // the handler). forsqlite's per-connection lock (acquire_conn/release_conn)
    // must serialize the offloaded steps so neither query corrupts the other —
    // both still return the right count, and the server doesn't deadlock.
    let Some(forsqlite) = forsqlite_dir() else {
        eprintln!("skipping: forsqlite checkout not found");
        return;
    };
    let server = boot_server(&forsqlite);
    let p = server.port;
    let handles: Vec<_> = (0..2)
        .map(|_| thread::spawn(move || get_path(p, "/shared")))
        .collect();
    for h in handles {
        let resp = h.join().unwrap();
        assert!(
            resp.contains("200") && resp.contains("2000000"),
            "shared-connection query wrong/corrupted:\n{resp}"
        );
    }
}

#[test]
fn concurrent_sqlite_queries_overlap() {
    let Some(forsqlite) = forsqlite_dir() else {
        eprintln!("skipping: forsqlite checkout not found");
        return;
    };
    let server = boot_server(&forsqlite);
    let p = server.port;

    // Warm + measure a single query.
    let (single, resp) = timed_get(p);
    assert!(resp.contains("2000000"), "single query wrong: {resp}");

    // Two concurrent queries (each its own :memory: db). The blocking step
    // offloads to the boundary, so they run on separate worker threads and
    // overlap rather than serialize.
    let start = Instant::now();
    let handles: Vec<_> = (0..2)
        .map(|_| thread::spawn(move || timed_get(p).1))
        .collect();
    for h in handles {
        assert!(h.join().unwrap().contains("2000000"));
    }
    let total = start.elapsed();

    // Serial would be ~2x a single query; overlapped is ~1x. Allow generous
    // slack for scheduling noise but stay well under 2x.
    assert!(
        total < single * 3 / 2 + Duration::from_millis(100),
        "expected concurrent SQLite queries to overlap: single={single:?}, two={total:?}"
    );
}
