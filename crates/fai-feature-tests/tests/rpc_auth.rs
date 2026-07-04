//! Plan 133 phase 2 acceptance: @auth enforcement at the dispatch
//! boundary, over real HTTP.
//!
//! public is reachable unauthenticated; session rejects with 401 BEFORE
//! the handler body runs (the body would loudly print if reached);
//! authorizer failure is 403; the authorizer passes for the right
//! caller. Envelopes are the fixed value-free strings.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

const SERVER_SOURCE: &str = r#"use std.http.server

use std.dictionary

use std.string

use { authorizer, caller, interfaceMode, onResolveIdentity } from Forui.rpc

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
        print('covered by rpc_auth.rs')
    end
end

# Only callers named admin pass.
def isAdmin
    @param ctx Dictionary
    @param argsJson String
    @return Bool
do
    let identity = dictionary.get(ctx, 'identity')
    if identity?
        let d Dictionary = identity!
        unwrap(getString(d, 'name'), '') == 'admin'
    else
        false
    end
end

test isAdmin
    it 'passes only the admin identity'
        assert.isTrue(isAdmin({ identity: { name: 'admin' } }, '[]'))
        assert.isFalse(isAdmin({ identity: { name: 'alice' } }, '[]'))
        assert.isFalse(isAdmin({}, '[]'))
    end
end

# Open endpoint.
remote def ping
    @auth public
    @return String
do
    'pong'
end

test ping
    it 'answers pong'
        assert.equals(ping(), 'pong')
    end
end

# Session-gated endpoint. The print marks body entry so the harness can
# prove a 401 never reached user code.
remote def whoami
    @auth session
    @return String
do
    print('BODY-ENTERED:whoami')
    let c = caller()
    if c != null
        let d Dictionary = c
        unwrap(getString(d, 'name'), 'missing')
    else
        'anonymous'
    end
end

test whoami
    it 'reports anonymous outside a request'
        assert.equals(whoami(), 'anonymous')
    end
end

# Admin-only endpoint.
remote def nuke
    @auth session, role: 'admin'
    @return String
do
    print('BODY-ENTERED:nuke')
    'boom'
end

test nuke
    it 'answers boom when called directly'
        assert.equals(nuke(), 'boom')
    end
end

# Upper-case an argument (arity-1 endpoint for arg validation tests).
remote def echoUpper
    @param text String
    @auth session
    @return String
do
    string.toUpper(text)
end

test echoUpper
    it 'upper-cases'
        assert.equals(echoUpper('ab'), 'AB')
    end
end

def main
    @return Void
do
    interfaceMode('public')
    onResolveIdentity(resolveTestIdentity)
    authorizer('admin', isAdmin)
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
        "rpc_auth_{}_{}",
        std::process::id(),
        port
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("fai.toml"),
        "[project]\nname = \"RpcAuth\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
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

fn rpc_call(port: u16, function: &str, user: Option<&str>, hash: &str) -> String {
    rpc_call_args(port, function, "[]", user, hash)
}

fn rpc_call_args(port: u16, function: &str, args: &str, user: Option<&str>, hash: &str) -> String {
    let body = format!(
        r#"{{"fn":"{}","args":{},"hash":"{}"}}"#,
        function, args, hash
    );
    let user_header = match user {
        Some(u) => format!("x-test-user: {}\r\n", u),
        None => String::new(),
    };
    let request = format!(
        "POST /fai/rpc HTTP/1.1\r\nHost: x\r\n{}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        user_header,
        body.len(),
        body
    );
    http(port, &request)
}

fn status_line(resp: &str) -> &str {
    resp.lines().next().unwrap_or("")
}

#[test]
fn auth_policies_enforced_at_dispatch_boundary() {
    let port = free_port();
    let mut server = boot_server(port);
    let hash = interface_hash(port);

    // public: reachable unauthenticated.
    let resp = rpc_call(port, "ping", None, &hash);
    assert!(status_line(&resp).contains("200"), "public: {}", resp);
    assert!(resp.contains(r#"{"ok":true,"value":"pong"}"#), "{}", resp);

    // session unauthenticated: 401, fixed envelope, no detail.
    let resp = rpc_call(port, "whoami", None, &hash);
    assert!(status_line(&resp).contains("401"), "session unauth: {}", resp);
    assert!(
        resp.contains(r#"{"ok":false,"authRequired":true,"error":"authentication required"}"#),
        "{}",
        resp
    );

    // session authenticated: 200 with the caller's identity.
    let resp = rpc_call(port, "whoami", Some("alice"), &hash);
    assert!(status_line(&resp).contains("200"), "session auth: {}", resp);
    assert!(resp.contains(r#"{"ok":true,"value":"alice"}"#), "{}", resp);

    // authorizer: authenticated non-admin is 403; admin passes.
    let resp = rpc_call(port, "nuke", Some("alice"), &hash);
    assert!(status_line(&resp).contains("403"), "authorizer deny: {}", resp);
    assert!(
        resp.contains(r#"{"ok":false,"authForbidden":true,"error":"forbidden"}"#),
        "{}",
        resp
    );
    let resp = rpc_call(port, "nuke", Some("admin"), &hash);
    assert!(status_line(&resp).contains("200"), "authorizer allow: {}", resp);
    assert!(resp.contains(r#"{"ok":true,"value":"boom"}"#), "{}", resp);

    // Unauthenticated nuke: 401 (authn precedes authz).
    let resp = rpc_call(port, "nuke", None, &hash);
    assert!(status_line(&resp).contains("401"), "authz unauth: {}", resp);

    // Arg validation (phase 3): wrong arity and non-JSON args answer the
    // fixed 400 envelope; a valid call still works.
    let resp = rpc_call_args(port, "echoUpper", "[1,2]", Some("alice"), &hash);
    assert!(status_line(&resp).contains("400"), "arity: {}", resp);
    assert!(
        resp.contains(r#"{"ok":false,"badRequest":true,"error":"bad request"}"#),
        "{}",
        resp
    );
    let resp = rpc_call_args(port, "echoUpper", r#""notjson""#, Some("alice"), &hash);
    assert!(status_line(&resp).contains("400"), "malformed: {}", resp);
    let resp = rpc_call_args(port, "echoUpper", r#"["hey"]"#, Some("alice"), &hash);
    assert!(status_line(&resp).contains("200"), "valid args: {}", resp);
    assert!(resp.contains(r#"{"ok":true,"value":"HEY"}"#), "{}", resp);

    // The 401/403 rejections must never have entered the handler bodies:
    // exactly TWO body-entry prints (authed whoami, admin nuke) plus the
    // two from the pipeline's test step.
    let _ = server.child.kill();
    let _ = server.child.wait();
    let mut out = String::new();
    if let Some(mut s) = server.child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    let whoami_entries = out.matches("BODY-ENTERED:whoami").count();
    let nuke_entries = out.matches("BODY-ENTERED:nuke").count();
    assert_eq!(
        whoami_entries, 2,
        "whoami body must run once in tests + once authed.\nstdout:\n{}",
        out
    );
    assert_eq!(
        nuke_entries, 2,
        "nuke body must run once in tests + once as admin.\nstdout:\n{}",
        out
    );
}
