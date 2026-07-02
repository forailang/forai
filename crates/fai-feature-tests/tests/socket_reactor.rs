//! Plan 103 U5: stdlib socket waits are readiness-driven — many parked
//! `tcp.read`s cost zero waiter threads, and each resumes with its own
//! connection's data (no cross-talk).

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

const CONNS: usize = 20;

fn reader_swarm_source(port: u16) -> String {
    format!(
        "use std.net.tcp\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 var i = 0\n\
         \x20 while i < {conns}\n\
         \x20   let conn = tcp.connect('127.0.0.1', {port})\n\
         \x20   nowait reader(conn)\n\
         \x20   i = i + 1\n\
         \x20 end\n\
         \x20 sleep(4000)\n\
         end\n\
         \n\
         private:\n\
         \n\
         # Parks on one connection and reports its payload.\n\
         def reader\n\
         \x20   @param conn Int\n\
         \x20   @return Void\n\
         do\n\
         \x20 let data = tcp.read(conn)\n\
         \x20 print('got ' + data)\n\
         \x20 tcp.close(conn)\n\
         end\n\
         \n\
         test reader\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n",
        conns = CONNS,
        port = port,
    )
}

/// Count live boundary waiter threads (named `fai-wait-*`) in the child.
/// Total thread count is useless here — wasmtime's parallel compilation pool
/// alone is ~cpu-count threads; the waiter names are the signal.
fn waiter_thread_count(pid: u32) -> Option<usize> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut count = 0;
    for task in tasks.flatten() {
        if let Ok(comm) = std::fs::read_to_string(task.path().join("comm")) {
            if comm.trim_end().starts_with("fai-wait") {
                count += 1;
            }
        }
    }
    Some(count)
}

struct ChildGuard(Option<Child>);
impl ChildGuard {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("child taken")
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn parked_tcp_reads_use_no_waiter_threads_and_resume_without_crosstalk() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let dir = workspace_root()
        .join("target")
        .join("tmp")
        .join(format!("socket_reactor_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("main.fai");
    std::fs::write(&src, reader_swarm_source(port)).unwrap();

    let child = Command::new(fai_binary())
        .arg("run")
        .arg(&src)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fai run");
    let pid = child.id();
    let mut guard = ChildGuard(Some(child));

    // Accept all connections (the program connects during its accept loop).
    let mut accepted: Vec<TcpStream> = Vec::new();
    listener
        .set_nonblocking(false)
        .expect("blocking accept mode");
    let deadline = Instant::now() + Duration::from_secs(30);
    listener
        .set_nonblocking(true)
        .expect("nonblocking accept mode");
    while accepted.len() < CONNS && Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => accepted.push(stream),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
        if let Ok(Some(status)) = guard.child().try_wait() {
            panic!("fai exited early: {status}");
        }
    }
    assert_eq!(accepted.len(), CONNS, "not all readers connected");

    // Give the readers a beat to park on their reads, then count waiters.
    thread::sleep(Duration::from_millis(400));
    if let Some(waiters) = waiter_thread_count(pid) {
        // Pre-U5 each parked read held a `fai-wait-*` thread (20 here).
        // Post-U5 the reads are readiness-parked; at most a transient
        // connect waiter might still be exiting.
        assert!(
            waiters < 3,
            "expected readiness-parked reads to hold no waiter threads, found {waiters}"
        );
    }

    // Distinct payload per connection; each reader must report its own.
    for (i, stream) in accepted.iter_mut().enumerate() {
        stream
            .write_all(format!("payload-{i}").as_bytes())
            .expect("write payload");
    }

    let output = guard
        .0
        .take()
        .expect("child taken")
        .wait_with_output()
        .expect("wait for fai");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for i in 0..CONNS {
        let needle = format!("got payload-{i}");
        let count = stdout.lines().filter(|l| l.trim() == needle).count();
        assert_eq!(
            count, 1,
            "expected exactly one '{needle}' line (cross-talk or lost read):\n{stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
