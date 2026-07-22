//! RPC handler bodies that suspend on host HTTP must complete.
//!
//! Brain first-run exercises this shape: `/fai/rpc` dispatches a public
//! `remote def`, and that remote body calls `std.http.request` before returning
//! the RPC envelope.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, forui_dir, workspace_root};

fn server_source(port: u16, stub_port: u16) -> String {
    format!(
        r#"use std.http.server

use std.http.request

use {{ interfaceMode }} from Forui.rpc

# Fetch from a local HTTP service through std.http.request.
remote def fetchStub
    @auth public
    @return String
do
    let response = request.get('http://127.0.0.1:{stub_port}/status')
    if is_null(response)
        'null'
    else
        response.body
    end
end

test fetchStub
    it 'is exercised by rpc_host_io.rs'
        print('covered by Rust integration test')
    end
end

def main
    @return Void
do
    interfaceMode('public')
    var r = server.router()
    addRpcRoutes(r)
    server.listen(r, {port})
end
"#
    )
}

struct ServerProc {
    child: std::process::Child,
    dir: std::path::PathBuf,
}

impl ServerProc {
    fn terminate_and_output(&mut self) -> (String, String) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut out = String::new();
        if let Some(mut s) = self.child.stdout.take() {
            let _ = s.read_to_string(&mut out);
        }
        let mut err = String::new();
        if let Some(mut s) = self.child.stderr.take() {
            let _ = s.read_to_string(&mut err);
        }
        (out, err)
    }
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

fn boot_stub_server() -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicBool::new(false));
    let accepted_thread = Arc::clone(&accepted);
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            accepted_thread.store(true, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = "stub-ok";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, accepted)
}

fn boot_server(port: u16, stub_port: u16) -> ServerProc {
    let dir = workspace_root().join("target").join("tmp").join(format!(
        "rpc_host_io_{}_{}",
        std::process::id(),
        port
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("fai.toml"),
        format!(
            "[project]\nname = \"RpcHostIo\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
             [project.server]\ntarget = \"wasm\"\nmain = \"src/main.fai\"\nrpc_server = true\n\n\
             [dependencies]\nForui = \"file://{}\"\n",
            forui_dir().display()
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/main.fai"), server_source(port, stub_port)).unwrap();

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
    stream.read_to_string(&mut resp).expect("read response");
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

#[test]
fn rpc_handler_can_await_host_http_request() {
    let port = free_port();
    let (stub_port, stub_accepted) = boot_stub_server();
    let mut server = boot_server(port, stub_port);
    let hash = interface_hash(port);
    let body = format!(r#"{{"fn":"fetchStub","args":[],"hash":"{}"}}"#, hash);
    let request = format!(
        "POST /fai/rpc HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );

    let response = match std::panic::catch_unwind(|| http(port, &request)) {
        Ok(response) => response,
        Err(err) => {
            let (out, server_err) = server.terminate_and_output();
            let src = std::fs::read_to_string(server.dir.join("src/main.fai"))
                .unwrap_or_else(|e| format!("read generated source failed: {e}"));
            panic!(
                "RPC request did not complete\nstub_accepted={}\nsource:\n{src}\nstdout:\n{out}\nstderr:\n{server_err}\nerror: {err:?}",
                stub_accepted.load(Ordering::SeqCst)
            );
        }
    };

    assert!(response.contains("200"), "expected 200, got:\n{response}");
    assert!(
        response.contains(r#"{"ok":true,"value":"stub-ok"}"#),
        "expected RPC response to include stub body, got:\n{response}"
    );
}
