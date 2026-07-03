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

/// Materialize a throwaway project with the given fai.toml and main.fai.
fn write_project_with_toml(tag: &str, toml: &str, main_body: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fai_secrets_egress_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("mkdir project");
    std::fs::write(root.join("fai.toml"), toml).expect("write fai.toml");
    std::fs::write(root.join("src/main.fai"), main_body).expect("write main.fai");
    root
}

/// Materialize a throwaway project with the default env-backend manifest.
fn write_project(tag: &str, main_body: &str) -> std::path::PathBuf {
    write_project_with_toml(
        tag,
        "[project]\nname = \"SecretsEgress\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
         [secrets]\nEGRESS_TOKEN = { required = true }\n",
        main_body,
    )
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

/// AWS backend end-to-end through `fai run`: the manifest's [secrets.aws]
/// endpoint points at a mock Secrets Manager; startup validation fetches
/// the declared secret with a SigV4-signed GetSecretValue, and
/// `secrets.reveal` returns the cached value.
#[test]
fn aws_backend_fetches_through_mock_secrets_manager() {
    // Mock Secrets Manager: one signed GetSecretValue request.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock sm");
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut collected = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            collected.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&collected);
            if let Some(header_end) = text.find("\r\n\r\n") {
                let content_length = text
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if collected.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        let body = r#"{"Name":"app/AWS_TEST_TOKEN","SecretString":"aws-mock-value"}"#;
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/x-amz-json-1.1\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        );
        String::from_utf8_lossy(&collected).into_owned()
    });

    let toml = format!(
        "[project]\nname = \"SecretsAws\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
         [secrets]\nbackend = \"aws\"\nAWS_TEST_TOKEN = {{ required = true }}\n\n\
         [secrets.aws]\nregion = \"us-east-1\"\nprefix = \"app/\"\n\
         endpoint = \"http://127.0.0.1:{}\"\n",
        port
    );
    let root = write_project_with_toml(
        "aws",
        &toml,
        "use std.secrets\n\
         \n\
         def main\n\
         \x20   @return Void\n\
         do\n\
         \x20   print(secrets.reveal(secrets.get('AWS_TEST_TOKEN')))\n\
         end\n",
    );

    let out = Command::new(fai_binary())
        .arg("run")
        .current_dir(&root)
        .env("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE")
        .env("AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")
        .output()
        .expect("spawn fai run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "fai run failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("aws-mock-value"),
        "reveal should return the fetched value.\nstdout: {}",
        stdout
    );

    let request = server.join().expect("mock sm thread");
    assert!(
        request.contains(r#"{"SecretId":"app/AWS_TEST_TOKEN"}"#),
        "prefix mapping missing.\nrequest: {}",
        request
    );
    assert!(
        request
            .to_lowercase()
            .contains("authorization: aws4-hmac-sha256 credential=akidexample/"),
        "sigv4 header missing.\nrequest: {}",
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
