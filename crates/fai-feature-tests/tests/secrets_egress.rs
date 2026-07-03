//! Plan 132 phase 3 end-to-end egress proof.
//!
//! Boots a host-side capture server, runs a real fai project whose only
//! credential access is `secrets.bearer(secrets.get(...))` in an HTTP
//! header, and asserts BOTH halves of the never-in-guest-memory property:
//!
//!   1. the resolved plaintext (`Bearer <value>`) reached the wire — the
//!      host spliced it in at egress;
//!   2. the guest's linear memory never contained the value — enforced by
//!      the runner's `FAI_ASSERT_NOT_IN_GUEST_MEMORY` raw byte scan, which
//!      catches even freed-but-not-overwritten plaintext.
//!
//! A control case proves the scan hook actually bites: the same program
//! with an added `secrets.reveal(...)` MUST fail the scan.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

use fai_feature_tests::fai_binary;

const SECRET_VALUE: &str = "egress-plaintext-hunter2";

/// One-shot HTTP capture server: accept a single connection, read the
/// request, answer 200, hand back the raw request text.
fn capture_server() -> (u16, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind capture server");
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = vec![0u8; 16384];
        let n = stream.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
        );
        request
    });
    (port, handle)
}

/// Materialize a throwaway project with a [secrets] manifest and the
/// given main.fai body.
fn write_project(tag: &str, main_body: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fai_secrets_egress_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("mkdir project");
    std::fs::write(
        root.join("fai.toml"),
        "[project]\nname = \"SecretsEgress\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
         [secrets]\nEGRESS_TOKEN = { required = true }\n",
    )
    .expect("write fai.toml");
    std::fs::write(root.join("src/main.fai"), main_body).expect("write main.fai");
    root
}

fn run_project(root: &std::path::Path, port: u16) -> std::process::Output {
    Command::new(fai_binary())
        .arg("run")
        .current_dir(root)
        .env("EGRESS_TOKEN", SECRET_VALUE)
        .env("FAI_TEST_EGRESS_URL", format!("http://127.0.0.1:{}", port))
        .env("FAI_ASSERT_NOT_IN_GUEST_MEMORY", SECRET_VALUE)
        .output()
        .expect("spawn fai run")
}

#[test]
fn secret_header_resolves_at_egress_without_entering_guest_memory() {
    let (port, server) = capture_server();
    let root = write_project(
        "header",
        "use std.secrets\n\
         \n\
         use std.env\n\
         \n\
         use std.http.request\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20   let url = env.get('FAI_TEST_EGRESS_URL')!\n\
         \x20   let key = secrets.get('EGRESS_TOKEN')\n\
         \x20   let response = request.post(url, 'payload', {\n\
         \x20       'Authorization': secrets.bearer(key)\n\
         \x20   })\n\
         \x20   print(response.status)\n\
         end\n",
    );

    let out = run_project(&root, port);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "fai run failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("200"),
        "expected the guest to see status 200.\nstdout: {}",
        stdout
    );

    let request = server.join().expect("capture server thread");
    // ureq lowercases header names on the wire.
    assert!(
        request
            .to_lowercase()
            .contains(&format!("authorization: bearer {}", SECRET_VALUE.to_lowercase())),
        "resolved bearer header did not reach the wire.\nrequest: {}",
        request
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Control: the same program with an explicit `secrets.reveal` DOES put
/// plaintext in guest memory, and the raw scan must catch it — proving
/// the passing test above isn't a vacuous assertion.
#[test]
fn reveal_control_case_fails_the_guest_memory_scan() {
    let (port, server) = capture_server();
    let root = write_project(
        "control",
        "use std.secrets\n\
         \n\
         use std.env\n\
         \n\
         use std.http.request\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20   let url = env.get('FAI_TEST_EGRESS_URL')!\n\
         \x20   let key = secrets.get('EGRESS_TOKEN')\n\
         \x20   let plain = secrets.reveal(key)\n\
         \x20   let response = request.post(url, 'payload', {\n\
         \x20       'Authorization': 'Bearer ' + plain\n\
         \x20   })\n\
         \x20   print(response.status)\n\
         end\n",
    );

    let out = run_project(&root, port);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "reveal control case should FAIL the memory scan.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    assert!(
        format!("{}{}", stdout, stderr).contains("assert-not-in-guest-memory"),
        "expected the scan failure marker.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // The request still went out (with plaintext) before the exit scan.
    let request = server.join().expect("capture server thread");
    assert!(
        request.contains(SECRET_VALUE),
        "control case request should carry plaintext.\nrequest: {}",
        request
    );

    let _ = std::fs::remove_dir_all(&root);
}
