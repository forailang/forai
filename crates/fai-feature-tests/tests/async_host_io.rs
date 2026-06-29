//! Characterization tests for blocking host I/O inside scheduler tasks.
//!
//! `nowait` is cooperative: sibling tasks can only run when the current task
//! yields. Blocking stdlib host calls therefore need to lower through the async
//! boundary, like `remoteCall` and supported FFI, instead of running inline on
//! the scheduler thread.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use fai_feature_tests::{fai_binary, workspace_root};

const HTTP_DELAY_MS: u64 = 600;

fn spawn_delayed_http_stub(delay: Duration) -> (u16, Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let _ = tx.send(());
                thread::sleep(delay);
                let body = "slow response";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });
    (port, rx)
}

fn spawn_capture_http_stub() -> (u16, Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    request.extend_from_slice(&buf[..n]);
                    if complete_http_request(&request) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let captured = String::from_utf8_lossy(&request).into_owned();
        let _ = tx.send(captured);
        let body = "posted";
        let resp = format!(
            "HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nx-reply: ok\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
    });
    (port, rx)
}

fn spawn_status_http_stub(
    status: u16,
    reason: &'static str,
    body: &'static str,
) -> (u16, Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let _ = tx.send(());
        let resp = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
    });
    (port, rx)
}

fn spawn_delayed_tcp_client(port: u16, delay: Duration) -> Receiver<()> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        thread::sleep(delay);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                let _ = tx.send(());
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    rx
}

fn spawn_delayed_tcp_line_server(delay: Duration, line: &'static str) -> (u16, Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = tx.send(());
        thread::sleep(delay);
        let _ = stream.write_all(line.as_bytes());
    });
    (port, rx)
}

fn spawn_delayed_udp_sender(port: u16, delay: Duration, data: &'static str) -> Receiver<()> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        thread::sleep(delay);
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut reported = false;
        while Instant::now() < deadline {
            if socket.send_to(data.as_bytes(), ("127.0.0.1", port)).is_ok() && !reported {
                let _ = tx.send(());
                reported = true;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    rx
}

fn complete_http_request(request: &[u8]) -> bool {
    let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let header_text = String::from_utf8_lossy(&request[..header_end]);
    let content_len = header_text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    request.len() >= header_end + 4 + content_len
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn tmp_dir(prefix: &str, port: u16) -> std::path::PathBuf {
    workspace_root().join("target").join("tmp").join(format!(
        "{prefix}_{}_{}",
        std::process::id(),
        port
    ))
}

fn run_fai_source(prefix: &str, port: u16, source: &str) -> Output {
    run_fai_source_with_args(prefix, port, source, &[])
}

fn run_fai_source_with_args(prefix: &str, port: u16, source: &str, extra_args: &[&str]) -> Output {
    run_fai_source_with_args_timeout(prefix, port, source, extra_args, Duration::from_secs(20))
}

fn run_fai_source_with_args_timeout(
    prefix: &str,
    port: u16,
    source: &str,
    extra_args: &[&str],
    timeout: Duration,
) -> Output {
    let dir = tmp_dir(prefix, port);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("main.fai");
    std::fs::write(&src, source).unwrap();
    let mut cmd = Command::new(fai_binary());
    cmd.arg("run").arg(&src);
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn fai run");
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(_status) = child.try_wait().expect("poll fai run") {
            let output = child.wait_with_output().expect("collect fai run output");
            let _ = std::fs::remove_dir_all(&dir);
            return output;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect timed out fai run");
            let _ = std::fs::remove_dir_all(&dir);
            panic!(
                "fai run timed out after {timeout:?}\nstdout:\n{}\nstderr:\n{}",
                stdout_string(&output),
                stderr_string(&output)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn run_fai_source_timeout(prefix: &str, port: u16, source: &str, timeout: Duration) -> Output {
    run_fai_source_with_args_timeout(prefix, port, source, &[], timeout)
}

fn nowait_tcp_accept_source(port: u16) -> String {
    format!(
        "use std.convert\n\
         use std.net.tcp\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let listener = tcp.listen({port})\n\
         \x20 nowait acceptWorker(listener)\n\
         \x20 nowait loggerWorker()\n\
         \x20 sleep(900)\n\
         \x20 tcp.close(listener)\n\
         end\n\
         \n\
         private:\n\
         \n\
         # Accepts one delayed TCP client.\n\
         def acceptWorker\n\
         \x20   @param listener Int\n\
         \x20   @return Void\n\
         do\n\
         \x20 print('accept start')\n\
         \x20 let accepted = tcp.accept(listener)\n\
         \x20 print('accept handle=' + convert.toString(getInt(accepted, 'handle')!))\n\
         end\n\
         \n\
         test acceptWorker\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n\
         \n\
         # Logs once so the test can observe scheduler interleaving.\n\
         def loggerWorker\n\
         \x20   @return Void\n\
         do\n\
         \x20 print('logger start')\n\
         end\n\
         \n\
         test loggerWorker\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n",
        port = port,
    )
}

fn nowait_tcp_read_line_source(port: u16) -> String {
    format!(
        "use std.net.tcp\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let conn = tcp.connect('127.0.0.1', {port})\n\
         \x20 nowait readWorker(conn)\n\
         \x20 nowait loggerWorker()\n\
         \x20 sleep(900)\n\
         \x20 tcp.close(conn)\n\
         end\n\
         \n\
         private:\n\
         \n\
         # Reads a delayed line from the TCP peer.\n\
         def readWorker\n\
         \x20   @param conn Int\n\
         \x20   @return Void\n\
         do\n\
         \x20 print('read start')\n\
         \x20 let line = tcp.readLine(conn)\n\
         \x20 print('read done=' + line)\n\
         end\n\
         \n\
         test readWorker\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n\
         \n\
         # Logs once so the test can observe scheduler interleaving.\n\
         def loggerWorker\n\
         \x20   @return Void\n\
         do\n\
         \x20 print('logger start')\n\
         end\n\
         \n\
         test loggerWorker\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n",
        port = port,
    )
}

fn nowait_udp_receive_source(port: u16) -> String {
    format!(
        "use std.net.udp\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let socket = udp.bind({port})\n\
         \x20 nowait receiveWorker(socket)\n\
         \x20 nowait loggerWorker()\n\
         \x20 sleep(900)\n\
         end\n\
         \n\
         private:\n\
         \n\
         # Receives one delayed UDP datagram.\n\
         def receiveWorker\n\
         \x20   @param socket Int\n\
         \x20   @return Void\n\
         do\n\
         \x20 print('udp start')\n\
         \x20 let packet = udp.receive(socket)\n\
         \x20 print('udp done=' + getString(packet, 'data')!)\n\
         end\n\
         \n\
         test receiveWorker\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n\
         \n\
         # Logs once so the test can observe scheduler interleaving.\n\
         def loggerWorker\n\
         \x20   @return Void\n\
         do\n\
         \x20 print('logger start')\n\
         end\n\
         \n\
         test loggerWorker\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n",
        port = port,
    )
}

fn close_pending_tcp_accept_source(port: u16) -> String {
    format!(
        "use std.convert\n\
         use std.net.tcp\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let listener = tcp.listen({port})\n\
         \x20 nowait acceptWorker(listener)\n\
         \x20 sleep(120)\n\
         \x20 tcp.close(listener)\n\
         \x20 print('closed listener')\n\
         \x20 sleep(500)\n\
         end\n\
         \n\
         private:\n\
         \n\
         # Accept should finish with null when the listener is closed while pending.\n\
         def acceptWorker\n\
         \x20   @param listener Int\n\
         \x20   @return Void\n\
         do\n\
         \x20 print('accept start')\n\
         \x20 let accepted = tcp.accept(listener)\n\
         \x20 let handle = getInt(accepted, 'handle')\n\
         \x20 print('accept null=' + convert.toString(handle == null))\n\
         end\n\
         \n\
         test acceptWorker\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n",
        port = port,
    )
}

fn nowait_http_source(stub_port: u16) -> String {
    format!(
        "use std.http.request\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 nowait httpWorker()\n\
         \x20 nowait loggerWorker()\n\
         \x20 sleep({wait_ms})\n\
         end\n\
         \n\
         private:\n\
         \n\
         # Starts a delayed outbound HTTP request.\n\
         def httpWorker\n\
         \x20   @return Void\n\
         do\n\
         \x20 print('http start')\n\
         \x20 let _resp = request.get('http://127.0.0.1:{stub_port}/slow')\n\
         \x20 print('http done')\n\
         end\n\
         \n\
         test httpWorker\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n\
         \n\
         # Logs once so the test can observe scheduler interleaving.\n\
         def loggerWorker\n\
         \x20   @return Void\n\
         do\n\
         \x20 print('logger start')\n\
         end\n\
         \n\
         test loggerWorker\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n",
        stub_port = stub_port,
        wait_ms = HTTP_DELAY_MS * 2,
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

fn server_source(port: u16, stub_port: u16) -> String {
    format!(
        "use std.http.request\n\
         use std.http.server\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let router = server.router()\n\
         \x20 nowait backgroundHttp()\n\
         \x20 server.get(router, '/quick') do with req HttpRequest\n\
         \x20   server.text(200, 'quick')\n\
         \x20 end\n\
         \x20 server.listen(router, {port})\n\
         end\n\
         \n\
         private:\n\
         \n\
         # Performs a delayed outbound HTTP request in the background.\n\
         def backgroundHttp\n\
         \x20   @return Void\n\
         do\n\
         \x20 let _resp = request.get('http://127.0.0.1:{stub_port}/slow')\n\
         end\n\
         \n\
         test backgroundHttp\n\
         \x20 it 'has a characterization test'\n\
         \x20   assert.isTrue(true)\n\
         \x20 end\n\
         end\n",
        port = port,
        stub_port = stub_port,
    )
}

fn post_http_source(stub_port: u16) -> String {
    format!(
        "use std.convert\n\
         use std.http.request\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let resp = request.post('http://127.0.0.1:{stub_port}/submit', 'payload', {{ XTest: 'fai' }})\n\
         \x20 print('status=' + convert.toString(resp.status))\n\
         \x20 print('body=' + resp.body)\n\
         \x20 print('reply=' + getString(resp.headers, 'x-reply')!)\n\
         end\n",
        stub_port = stub_port,
    )
}

fn post_form_http_source(stub_port: u16) -> String {
    format!(
        "use std.convert\n\
         use std.dictionary\n\
         use std.http.request\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 var headers = {{}}\n\
         \x20 headers = dictionary.set(headers, 'content-type', 'application/x-www-form-urlencoded')\n\
         \x20 let resp = request.post('http://127.0.0.1:{stub_port}/submit', 'a=b', headers)\n\
         \x20 print('status=' + convert.toString(resp.status))\n\
         end\n",
        stub_port = stub_port,
    )
}

fn http_status_source(stub_port: u16) -> String {
    format!(
        "use std.convert\n\
         use std.http.request\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let resp = request.get('http://127.0.0.1:{stub_port}/status')\n\
         \x20 print('status=' + convert.toString(resp.status))\n\
         \x20 print('body=' + resp.body)\n\
         end\n",
        stub_port = stub_port,
    )
}

fn nowait_process_source() -> String {
    "use std.process\n\
     \n\
     def main\n\
     \x20   @return Void\n\
     do\n\
     \x20 nowait processWorker()\n\
     \x20 nowait loggerWorker()\n\
     \x20 sleep(900)\n\
     end\n\
     \n\
     private:\n\
     \n\
     # Starts a slow process run so the scheduler can interleave another task.\n\
     def processWorker\n\
     \x20   @return Void\n\
     do\n\
     \x20 print('process start')\n\
     \x20 let _raw = process.run('sleep 0.4 && printf done', '.', '{}', 5000, 65536)\n\
     \x20 print('process done')\n\
     end\n\
     \n\
     test processWorker\n\
     \x20 it 'has a characterization test'\n\
     \x20   assert.isTrue(true)\n\
     \x20 end\n\
     end\n\
     \n\
     # Logs once so the test can observe scheduler interleaving.\n\
     def loggerWorker\n\
     \x20   @return Void\n\
     do\n\
     \x20 print('logger start')\n\
     end\n\
     \n\
     test loggerWorker\n\
     \x20 it 'has a characterization test'\n\
     \x20   assert.isTrue(true)\n\
     \x20 end\n\
     end\n"
        .to_string()
}

fn process_timeout_source() -> String {
    "use std.convert\n\
     use std.json\n\
     use std.process\n\
     \n\
     def main\n\
     \x20   @return Void\n\
     do\n\
     \x20 let raw = process.run('sleep 2', '.', '{}', 200, 65536)\n\
     \x20 let result Dictionary = json.parse(raw)\n\
     \x20 print('timed=' + convert.toString(getBool(result, 'timedOut')))\n\
     \x20 print('ok=' + convert.toString(getBool(result, 'ok')))\n\
     \x20 print('truncated=' + convert.toString(getBool(result, 'truncated')))\n\
     end\n"
        .to_string()
}

fn file_env_source(
    dir: &std::path::Path,
    input: &std::path::Path,
    output: &std::path::Path,
    dotenv: &std::path::Path,
    env_key: &str,
) -> String {
    format!(
        "use std.array\n\
         use std.convert\n\
         use std.env\n\
         use std.file\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let readBack = file.read('{input}')\n\
         \x20 print('read=' + readBack)\n\
         \x20 let writeOk = file.write('{output}', 'written')\n\
         \x20 print('write=' + convert.toString(writeOk))\n\
         \x20 let names = file.list('{dir}')\n\
         \x20 print('listed=' + convert.toString(array.contains(names, 'input.txt')))\n\
         \x20 let loaded = env.load('{dotenv}')\n\
         \x20 print('loaded=' + convert.toString(loaded))\n\
         \x20 print('env=' + env.get('{env_key}')!)\n\
         end\n",
        dir = dir.display(),
        input = input.display(),
        output = output.display(),
        dotenv = dotenv.display(),
        env_key = env_key,
    )
}

fn boot_server(source: &str, port: u16) -> ServerProc {
    let dir = tmp_dir("async_host_io_server", port);
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

fn timed_get(port: u16, path: &str) -> (Duration, String) {
    let start = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    (start.elapsed(), response)
}

fn wait_for_stub_request(rx: &Receiver<()>, label: &str) {
    rx.recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| panic!("timed out waiting for delayed stub request from {label}"));
}

fn stdout_string(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_string(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_log_before(stdout: &str, first: &str, second: &str, reason: &str) {
    let first_idx = stdout
        .find(first)
        .unwrap_or_else(|| panic!("missing '{first}' log:\n{stdout}"));
    let second_idx = stdout
        .find(second)
        .unwrap_or_else(|| panic!("missing '{second}' log:\n{stdout}"));
    assert!(first_idx < second_idx, "{reason}; stdout:\n{stdout}");
}

#[test]
fn nowait_http_request_yields_to_sibling_task() {
    let (stub_port, rx) = spawn_delayed_http_stub(Duration::from_millis(HTTP_DELAY_MS));
    let output = run_fai_source(
        "async_host_io_nowait",
        stub_port,
        &nowait_http_source(stub_port),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );
    wait_for_stub_request(&rx, "nowait worker");

    let stdout = stdout_string(&output);
    let http_done = stdout
        .find("http done")
        .unwrap_or_else(|| panic!("missing http completion log:\n{stdout}"));
    let logger_start = stdout
        .find("logger start")
        .unwrap_or_else(|| panic!("missing logger log:\n{stdout}"));
    assert!(
        logger_start < http_done,
        "expected sibling nowait task to run while HTTP was in flight; stdout:\n{stdout}"
    );
}

#[test]
fn background_http_request_does_not_block_quick_server_route() {
    let (stub_port, rx) = spawn_delayed_http_stub(Duration::from_millis(HTTP_DELAY_MS));
    let port = free_port();
    let server = boot_server(&server_source(port, stub_port), port);

    wait_for_stub_request(&rx, "background server worker");
    let (elapsed, response) = timed_get(server.port, "/quick");
    assert!(
        response.contains("200") && response.contains("quick"),
        "quick route failed:\n{response}"
    );
    assert!(
        elapsed < Duration::from_millis(HTTP_DELAY_MS / 2),
        "quick route waited behind background HTTP request: {elapsed:?}\n{response}"
    );
}

#[test]
fn async_http_post_preserves_body_headers_and_response_shape() {
    let (stub_port, rx) = spawn_capture_http_stub();
    let output = run_fai_source(
        "async_host_io_post",
        stub_port,
        &post_http_source(stub_port),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );

    let request = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("captured outbound POST request");
    assert!(
        request.starts_with("POST /submit HTTP/1.1"),
        "unexpected request line:\n{request}"
    );
    assert!(
        request.contains("XTest: fai") || request.contains("xtest: fai"),
        "missing copied request header:\n{request}"
    );
    assert!(
        request.ends_with("\r\n\r\npayload") || request.contains("\r\n\r\npayload"),
        "missing copied request body:\n{request}"
    );

    let stdout = stdout_string(&output);
    assert!(stdout.contains("status=201"), "missing status:\n{stdout}");
    assert!(stdout.contains("body=posted"), "missing body:\n{stdout}");
    assert!(
        stdout.contains("reply=ok"),
        "missing response header:\n{stdout}"
    );
}

#[test]
fn async_http_post_response_is_ownership_balanced() {
    let (stub_port, rx) = spawn_capture_http_stub();
    let output = run_fai_source_with_args(
        "async_host_io_post_ownership",
        stub_port,
        &post_http_source(stub_port),
        &["--check-ownership"],
    );
    assert!(
        output.status.success(),
        "fai run --check-ownership failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );

    let request = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("captured outbound POST request");
    assert!(
        request.starts_with("POST /submit HTTP/1.1"),
        "unexpected request line:\n{request}"
    );
}

#[test]
fn async_http_post_respects_caller_content_type() {
    let (stub_port, rx) = spawn_capture_http_stub();
    let output = run_fai_source(
        "async_host_io_post_content_type",
        stub_port,
        &post_form_http_source(stub_port),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );

    let request = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("captured outbound POST request");
    let lower = request.to_lowercase();
    assert!(
        lower.contains("content-type: application/x-www-form-urlencoded"),
        "missing form content type:\n{request}"
    );
    assert!(
        !lower.contains("application/json"),
        "default JSON content type should not be sent with caller content type:\n{request}"
    );
}

#[test]
fn async_http_error_status_returns_response_shape() {
    let (stub_port, rx) = spawn_status_http_stub(418, "I'm a Teapot", "short and stout");
    let output = run_fai_source(
        "async_host_io_http_status",
        stub_port,
        &http_status_source(stub_port),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );
    rx.recv_timeout(Duration::from_secs(5))
        .expect("stub received outbound request");

    let stdout = stdout_string(&output);
    assert!(stdout.contains("status=418"), "missing status:\n{stdout}");
    assert!(
        stdout.contains("body=short and stout"),
        "missing body:\n{stdout}"
    );
}

#[test]
fn nowait_process_run_yields_to_sibling_task() {
    let token = free_port();
    let output = run_fai_source(
        "async_host_io_process_nowait",
        token,
        &nowait_process_source(),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );

    let stdout = stdout_string(&output);
    let process_done = stdout
        .find("process done")
        .unwrap_or_else(|| panic!("missing process completion log:\n{stdout}"));
    let logger_start = stdout
        .find("logger start")
        .unwrap_or_else(|| panic!("missing logger log:\n{stdout}"));
    assert!(
        logger_start < process_done,
        "expected sibling nowait task to run while process.run was in flight; stdout:\n{stdout}"
    );
}

#[test]
fn async_process_run_timeout_preserves_json_shape() {
    let token = free_port();
    let output = run_fai_source(
        "async_host_io_process_timeout",
        token,
        &process_timeout_source(),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );

    let stdout = stdout_string(&output);
    assert!(stdout.contains("timed=true"), "missing timeout:\n{stdout}");
    assert!(stdout.contains("ok=false"), "missing ok=false:\n{stdout}");
    assert!(
        stdout.contains("truncated=false"),
        "missing truncated=false:\n{stdout}"
    );
}

#[test]
fn async_file_and_env_host_ops_preserve_values() {
    let token = free_port();
    let dir = tmp_dir("async_host_io_file_env_data", token);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("input.txt");
    let output_path = dir.join("output.txt");
    let dotenv = dir.join("fixture.env");
    let env_key = format!("FAI_U5_ENV_{}_{}", std::process::id(), token);
    std::fs::write(&input, "from-file").unwrap();
    std::fs::write(&dotenv, format!("{env_key}=from-env\n")).unwrap();

    let source = file_env_source(&dir, &input, &output_path, &dotenv, &env_key);
    let output = run_fai_source("async_host_io_file_env", token, &source);
    let cleanup_result = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );
    cleanup_result.unwrap();

    let stdout = stdout_string(&output);
    assert!(stdout.contains("read=from-file"), "missing read:\n{stdout}");
    assert!(stdout.contains("write=true"), "missing write:\n{stdout}");
    assert!(stdout.contains("listed=true"), "missing list:\n{stdout}");
    assert!(
        stdout.contains("loaded=true"),
        "missing env load:\n{stdout}"
    );
    assert!(
        stdout.contains("env=from-env"),
        "missing env get:\n{stdout}"
    );
}

#[test]
fn nowait_tcp_accept_yields_to_sibling_task() {
    let port = free_port();
    let rx = spawn_delayed_tcp_client(port, Duration::from_millis(300));
    let output = run_fai_source(
        "async_host_io_tcp_accept_nowait",
        port,
        &nowait_tcp_accept_source(port),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );
    rx.recv_timeout(Duration::from_secs(5))
        .expect("delayed TCP client connected");

    let stdout = stdout_string(&output);
    assert_log_before(
        &stdout,
        "logger start",
        "accept handle=",
        "expected sibling nowait task to run while tcp.accept was pending",
    );
}

#[test]
fn nowait_tcp_read_line_yields_to_sibling_task() {
    let (port, rx) = spawn_delayed_tcp_line_server(Duration::from_millis(300), "line-from-peer\n");
    let output = run_fai_source(
        "async_host_io_tcp_read_line_nowait",
        port,
        &nowait_tcp_read_line_source(port),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );
    rx.recv_timeout(Duration::from_secs(5))
        .expect("FAI TCP client connected to line server");

    let stdout = stdout_string(&output);
    assert_log_before(
        &stdout,
        "logger start",
        "read done=line-from-peer",
        "expected sibling nowait task to run while tcp.readLine was pending",
    );
}

#[test]
fn nowait_udp_receive_yields_to_sibling_task() {
    let port = free_port();
    let rx = spawn_delayed_udp_sender(port, Duration::from_millis(300), "packet-from-peer");
    let output = run_fai_source(
        "async_host_io_udp_receive_nowait",
        port,
        &nowait_udp_receive_source(port),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );
    rx.recv_timeout(Duration::from_secs(5))
        .expect("delayed UDP sender ran");

    let stdout = stdout_string(&output);
    assert_log_before(
        &stdout,
        "logger start",
        "udp done=packet-from-peer",
        "expected sibling nowait task to run while udp.receive was pending",
    );
}

#[test]
fn pending_tcp_accept_returns_null_when_listener_is_closed() {
    let port = free_port();
    let output = run_fai_source_timeout(
        "async_host_io_tcp_accept_close",
        port,
        &close_pending_tcp_accept_source(port),
        Duration::from_secs(8),
    );
    assert!(
        output.status.success(),
        "fai run failed\nstdout:\n{}\nstderr:\n{}",
        stdout_string(&output),
        stderr_string(&output)
    );

    let stdout = stdout_string(&output);
    assert!(
        stdout.contains("closed listener"),
        "main task did not close listener:\n{stdout}"
    );
    assert!(
        stdout.contains("accept null=true"),
        "pending accept did not finish with null after close:\n{stdout}"
    );
}

// A server whose request handler performs a slow outbound HTTP call, so the
// handler task parks on the boundary while the driver loop keeps running.
fn slow_handler_server_source(port: u16, stub_port: u16) -> String {
    format!(
        "use std.http.request\n\
         use std.http.server\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20 let router = server.router()\n\
         \x20 server.get(router, '/slow') do with req HttpRequest\n\
         \x20   let _resp = request.get('http://127.0.0.1:{stub_port}/slow')\n\
         \x20   server.text(200, 'done')\n\
         \x20 end\n\
         \x20 server.listen(router, {port})\n\
         end\n",
        port = port,
        stub_port = stub_port,
    )
}

// Regression test for the busy-wait fix: while a request handler is parked on a
// long outbound call, the server driver loop must wait for the next real event
// (boundary completion or nearest sleep-timer) instead of re-polling the guest
// scheduler every 1ms. We count polls rather than CPU because a minimal program's
// poll is cheap — the spin's cost only shows at scale (e.g. the brain server) —
// but the poll *rate* exposes the bug regardless of poll cost.
#[test]
fn server_handler_parked_on_outbound_call_does_not_busy_poll() {
    let (stub_port, _rx) = spawn_delayed_http_stub(Duration::from_millis(3000));
    let port = free_port();
    let dir = tmp_dir("async_host_io_pollcount", port);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("server.fai");
    std::fs::write(&src, slow_handler_server_source(port, stub_port)).unwrap();

    // Serve exactly one request then exit, printing the loop's total poll count.
    let mut child = Command::new(fai_binary())
        .arg("run")
        .arg(&src)
        .env("FAI_HTTP_MAX_REQUESTS", "1")
        .env("FAI_DEBUG_SERVER_POLLS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fai run");

    // Connect and send the request in one shot. With a 1-request cap we must not
    // use a throwaway readiness probe — that connection would consume the quota.
    let deadline = Instant::now() + Duration::from_secs(20);
    let start = Instant::now();
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) => {
                if let Ok(Some(status)) = child.try_wait() {
                    let mut err = String::new();
                    if let Some(mut s) = child.stderr.take() {
                        let _ = s.read_to_string(&mut err);
                    }
                    panic!("fai server exited early ({status}); stderr:\n{err}");
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("fai server did not start listening within 20s");
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("write request");
    // The handler parks ~3s waiting on the outbound stub before replying.
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read response");
    let dur = start.elapsed();
    assert!(resp.contains("done"), "handler did not complete:\n{resp}");
    assert!(
        dur >= Duration::from_millis(2500),
        "outbound delay was not actually awaited: {dur:?}"
    );

    let output = child.wait_with_output().expect("collect fai server output");
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = stderr_string(&output);
    let polls: u64 = stderr
        .lines()
        .find_map(|l| l.strip_prefix("__server_polls="))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no __server_polls marker in stderr:\n{stderr}"));

    // Lower bound: the loop must actually iterate while the handler is parked
    // (proving we exercised the parked-poll path, not a synchronous block).
    // Upper bound: a fixed-1ms re-poll would do ~3000 polls over the ~3s wait;
    // the timer-aware wait (≤250ms backstop, woken at completion) does ~12.
    assert!(
        (3..400).contains(&polls),
        "expected a parked-but-not-spinning poll count, got {polls} over {dur:?}"
    );
}
