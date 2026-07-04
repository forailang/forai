//! Plan 133 phase 1 acceptance gate: per-request RPC context does not
//! bleed across overlapping requests.
//!
//! Boots a real forui-based RPC server (Forui pulled via file://) whose
//! identity resolver reads the `x-test-user` header and whose handler
//! sleeps across a cooperative yield before reporting `rpc.caller()`.
//! Two concurrent calls with different identities must each see their
//! OWN identity — under the old module-global session pattern the
//! second request's identity overwrites the first mid-handler (the
//! plan-103 yield made that a real interleaving, not a theory).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

const SERVER_SOURCE: &str = r#"use std.http.server

use std.dictionary

use std.string

use { caller, onResolveIdentity } from Forui.rpc

# Resolve the caller from the x-test-user header (test-only seam).
def resolveTestIdentity
    @param request HttpRequest
    @return Unknown
do
    let user = getString(request.headers, 'x-test-user')
    if user?
        { name: user! }
    else
        null
    end
end

test resolveTestIdentity
    it 'is exercised end-to-end by the Rust harness'
        print('covered by rpc_context.rs')
    end
end

# Report the calling identity after yielding across a sleep.
remote def slowWhoami
    @auth session
    @return String
do
    sleep(300)
    let c = caller()
    if c != null
        let d Dictionary = c
        unwrap(getString(d, 'name'), 'missing')
    else
        'anonymous'
    end
end

test slowWhoami
    it 'reports anonymous outside a request'
        assert.equals(slowWhoami(), 'anonymous')
    end
end

def main
    @return Void
do
    onResolveIdentity(resolveTestIdentity)
    var r = server.router()
    addRpcRoutes(r)
    server.listen(r, PORT)
end
"#;

struct ServerProc {
    child: std::process::Child,
    dir: std::path::PathBuf,
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

fn boot_server(port: u16) -> ServerProc {
    let dir = workspace_root().join("target").join("tmp").join(format!(
        "rpc_context_{}_{}",
        std::process::id(),
        port
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("fai.toml"),
        "[project]\nname = \"RpcCtx\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
         [project.server]\ntarget = \"wasm\"\nmain = \"src/main.fai\"\nrpc_server = true\n\n\
         [dependencies]\nForui = \"file:///home/bal/forai/forui\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.fai"),
        SERVER_SOURCE.replace("PORT", &port.to_string()),
    )
    .unwrap();

    let child = Command::new(fai_binary())
        .arg("run")
        .arg("server")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fai run server");
    let mut proc = ServerProc { child, dir };

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return proc;
        }
        if let Ok(Some(status)) = proc.child.try_wait() {
            let mut err = String::new();
            if let Some(mut s) = proc.child.stderr.take() {
                let _ = s.read_to_string(&mut err);
            }
            let mut out = String::new();
            if let Some(mut s) = proc.child.stdout.take() {
                let _ = s.read_to_string(&mut out);
            }
            panic!("fai server exited early ({status});\nstdout:\n{out}\nstderr:\n{err}");
        }
        if Instant::now() >= deadline {
            panic!("fai server did not start listening within 60s");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn http(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    resp
}

fn interface_hash(port: u16) -> String {
    let resp = http(
        port,
        "GET /fai/interface HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let marker = "\"hash\":\"";
    let start = resp.find(marker).expect("interface hash") + marker.len();
    let end = resp[start..].find('"').unwrap() + start;
    resp[start..end].to_string()
}

fn rpc_call(port: u16, user: &str, hash: &str) -> String {
    let body = format!(
        r#"{{"fn":"slowWhoami","args":[],"hash":"{}"}}"#,
        hash
    );
    let request = format!(
        "POST /fai/rpc HTTP/1.1\r\nHost: x\r\nx-test-user: {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        user,
        body.len(),
        body
    );
    http(port, &request)
}

#[test]
fn concurrent_rpc_calls_keep_their_own_identity() {
    let port = free_port();
    let _server = boot_server(port);
    let hash = interface_hash(port);

    let started = Instant::now();
    let h_alice = {
        let hash = hash.clone();
        thread::spawn(move || rpc_call(port, "alice", &hash))
    };
    // Small stagger so bob's rpcBeginRequest lands while alice's handler
    // is parked in its sleep — the exact overwrite window of the old
    // module-global pattern.
    thread::sleep(Duration::from_millis(80));
    let h_bob = {
        let hash = hash.clone();
        thread::spawn(move || rpc_call(port, "bob", &hash))
    };

    let alice = h_alice.join().unwrap();
    let bob = h_bob.join().unwrap();
    let elapsed = started.elapsed();

    assert!(
        alice.contains(r#"{"ok":true,"value":"alice"}"#),
        "alice must see her own identity.\nresponse: {}",
        alice
    );
    assert!(
        bob.contains(r#"{"ok":true,"value":"bob"}"#),
        "bob must see his own identity.\nresponse: {}",
        bob
    );
    // Both handlers sleep 300ms; if the calls had serialized, elapsed
    // would exceed ~680ms. Overlap proves the bleed window was real.
    assert!(
        elapsed < Duration::from_millis(650),
        "calls should overlap (elapsed {:?})",
        elapsed
    );
}
