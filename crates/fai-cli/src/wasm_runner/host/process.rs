//! Process host imports for `std.process`.
//!
//! These functions intentionally return JSON strings instead of host-built
//! records. That keeps the stdlib surface small while still letting forai code
//! preserve full command status, stdout, stderr, and session metadata.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use wasmtime::*;

use super::super::heap::wasm_alloc_str;

const MAX_BUFFER_BYTES: usize = 1024 * 1024;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static SESSIONS: OnceLock<Mutex<HashMap<String, ShellSession>>> = OnceLock::new();

struct ShellSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: SharedBuffer,
    stderr: SharedBuffer,
    started: Instant,
    expires: Instant,
}

type SharedBuffer = Arc<Mutex<Vec<u8>>>;

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    linker
        .func_wrap(
            "env",
            "process_run",
            |mut caller: Caller<'_, ()>,
             command_ptr: i32,
             command_len: i32,
             cwd_ptr: i32,
             cwd_len: i32,
             env_ptr: i32,
             env_len: i32,
             timeout_ms: i32,
             max_output_bytes: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let (command, cwd, env_json) = {
                    let data = mem.data(&caller);
                    (
                        read_slice(data, command_ptr, command_len),
                        read_slice(data, cwd_ptr, cwd_len),
                        read_slice(data, env_ptr, env_len),
                    )
                };
                let result = run_command(
                    &command,
                    &cwd,
                    &env_json,
                    clamp_timeout_ms(timeout_ms),
                    clamp_output_bytes(max_output_bytes),
                );
                wasm_alloc_str(&mut caller, &mem, &result)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "process_start",
            |mut caller: Caller<'_, ()>,
             command_ptr: i32,
             command_len: i32,
             cwd_ptr: i32,
             cwd_len: i32,
             env_ptr: i32,
             env_len: i32,
             lifetime_ms: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let (command, cwd, env_json) = {
                    let data = mem.data(&caller);
                    (
                        read_slice(data, command_ptr, command_len),
                        read_slice(data, cwd_ptr, cwd_len),
                        read_slice(data, env_ptr, env_len),
                    )
                };
                cleanup_expired_sessions();
                let result =
                    start_session(&command, &cwd, &env_json, clamp_lifetime_ms(lifetime_ms));
                wasm_alloc_str(&mut caller, &mem, &result)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "process_write",
            |mut caller: Caller<'_, ()>,
             session_ptr: i32,
             session_len: i32,
             input_ptr: i32,
             input_len: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let (session_id, input) = {
                    let data = mem.data(&caller);
                    (
                        read_slice(data, session_ptr, session_len),
                        read_slice(data, input_ptr, input_len),
                    )
                };
                cleanup_expired_sessions();
                let result = write_session(&session_id, &input);
                wasm_alloc_str(&mut caller, &mem, &result)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "process_read",
            |mut caller: Caller<'_, ()>,
             session_ptr: i32,
             session_len: i32,
             max_output_bytes: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let session_id = {
                    let data = mem.data(&caller);
                    read_slice(data, session_ptr, session_len)
                };
                cleanup_expired_sessions();
                let result = read_session(&session_id, clamp_output_bytes(max_output_bytes));
                wasm_alloc_str(&mut caller, &mem, &result)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "process_stop",
            |mut caller: Caller<'_, ()>, session_ptr: i32, session_len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let session_id = {
                    let data = mem.data(&caller);
                    read_slice(data, session_ptr, session_len)
                };
                cleanup_expired_sessions();
                let result = stop_session(&session_id);
                wasm_alloc_str(&mut caller, &mem, &result)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

fn run_command(
    command: &str,
    cwd: &str,
    env_json: &str,
    timeout_ms: u64,
    max_output_bytes: usize,
) -> String {
    let started = Instant::now();
    let mut child = match command_builder(command, cwd, env_json)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return json!({
                "ok": false,
                "command": command,
                "cwd": cwd,
                "exitCode": null,
                "stdout": "",
                "stderr": e.to_string(),
                "timedOut": false,
                "durationMs": duration_ms(started),
                "truncated": false
            })
            .to_string();
        }
    };

    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(out) = child.stdout.take() {
        spawn_reader(out, stdout.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_reader(err, stderr.clone());
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let status = child.wait().ok();
                    break status.and_then(|s| s.code());
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break None,
        }
    };

    thread::sleep(Duration::from_millis(10));
    let (stdout_text, stdout_truncated) = drain_buffer(&stdout, max_output_bytes);
    let (stderr_text, stderr_truncated) = drain_buffer(&stderr, max_output_bytes);
    json!({
        "ok": !timed_out && exit_code == Some(0),
        "command": command,
        "cwd": cwd,
        "exitCode": exit_code,
        "stdout": stdout_text,
        "stderr": stderr_text,
        "timedOut": timed_out,
        "durationMs": duration_ms(started),
        "truncated": stdout_truncated || stderr_truncated
    })
    .to_string()
}

fn start_session(command: &str, cwd: &str, env_json: &str, lifetime_ms: u64) -> String {
    let mut child = match command_builder(command, cwd, env_json)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return json!({
                "ok": false,
                "sessionId": "",
                "command": command,
                "cwd": cwd,
                "error": e.to_string()
            })
            .to_string();
        }
    };

    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(out) = child.stdout.take() {
        spawn_reader(out, stdout.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_reader(err, stderr.clone());
    }
    let stdin = child.stdin.take();
    let id = format!("bash-{}", NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed));
    let now = Instant::now();
    sessions().lock().unwrap().insert(
        id.clone(),
        ShellSession {
            child,
            stdin,
            stdout,
            stderr,
            started: now,
            expires: now + Duration::from_millis(lifetime_ms),
        },
    );

    json!({
        "ok": true,
        "sessionId": id,
        "command": command,
        "cwd": cwd,
        "running": true,
        "lifetimeMs": lifetime_ms
    })
    .to_string()
}

fn write_session(session_id: &str, input: &str) -> String {
    let mut guard = sessions().lock().unwrap();
    let Some(session) = guard.get_mut(session_id) else {
        return json!({ "ok": false, "sessionId": session_id, "error": "session not found" })
            .to_string();
    };
    match session.stdin.as_mut() {
        Some(stdin) => match stdin
            .write_all(input.as_bytes())
            .and_then(|_| stdin.flush())
        {
            Ok(_) => {
                json!({ "ok": true, "sessionId": session_id, "bytes": input.len() }).to_string()
            }
            Err(e) => {
                json!({ "ok": false, "sessionId": session_id, "error": e.to_string() }).to_string()
            }
        },
        None => json!({ "ok": false, "sessionId": session_id, "error": "session stdin closed" })
            .to_string(),
    }
}

fn read_session(session_id: &str, max_output_bytes: usize) -> String {
    let mut guard = sessions().lock().unwrap();
    let Some(session) = guard.get_mut(session_id) else {
        return json!({ "ok": false, "sessionId": session_id, "error": "session not found" })
            .to_string();
    };
    let running = matches!(session.child.try_wait(), Ok(None));
    let exit_code = match session.child.try_wait() {
        Ok(Some(status)) => status.code(),
        _ => None,
    };
    let (stdout_text, stdout_truncated) = drain_buffer(&session.stdout, max_output_bytes);
    let (stderr_text, stderr_truncated) = drain_buffer(&session.stderr, max_output_bytes);
    json!({
        "ok": true,
        "sessionId": session_id,
        "running": running,
        "exitCode": exit_code,
        "stdout": stdout_text,
        "stderr": stderr_text,
        "durationMs": duration_ms(session.started),
        "truncated": stdout_truncated || stderr_truncated
    })
    .to_string()
}

fn stop_session(session_id: &str) -> String {
    let mut guard = sessions().lock().unwrap();
    let Some(mut session) = guard.remove(session_id) else {
        return json!({ "ok": false, "sessionId": session_id, "error": "session not found" })
            .to_string();
    };
    let running = matches!(session.child.try_wait(), Ok(None));
    if running {
        let _ = session.child.kill();
    }
    let status = session.child.wait().ok();
    json!({
        "ok": true,
        "sessionId": session_id,
        "stopped": running,
        "exitCode": status.and_then(|s| s.code())
    })
    .to_string()
}

fn command_builder(command: &str, cwd: &str, env_json: &str) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg("-lc").arg(command).current_dir(cwd);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(env_json) {
        if let Some(obj) = value.as_object() {
            for (key, value) in obj {
                if let Some(text) = value.as_str() {
                    cmd.env(key, text);
                }
            }
        }
    }
    cmd
}

fn spawn_reader<R>(mut reader: R, buffer: SharedBuffer)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => append_capped(&buffer, &chunk[..n]),
                Err(_) => break,
            }
        }
    });
}

fn append_capped(buffer: &SharedBuffer, bytes: &[u8]) {
    let mut guard = buffer.lock().unwrap();
    guard.extend_from_slice(bytes);
    if guard.len() > MAX_BUFFER_BYTES {
        let overflow = guard.len() - MAX_BUFFER_BYTES;
        guard.drain(0..overflow);
    }
}

fn drain_buffer(buffer: &SharedBuffer, max_output_bytes: usize) -> (String, bool) {
    let mut guard = buffer.lock().unwrap();
    let take = guard.len().min(max_output_bytes);
    let drained: Vec<u8> = guard.drain(0..take).collect();
    let truncated = !guard.is_empty();
    (String::from_utf8_lossy(&drained).into_owned(), truncated)
}

fn cleanup_expired_sessions() {
    let now = Instant::now();
    let expired: Vec<String> = {
        let guard = sessions().lock().unwrap();
        guard
            .iter()
            .filter_map(|(id, session)| {
                if session.expires <= now {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    };
    for id in expired {
        let _ = stop_session(&id);
    }
}

fn sessions() -> &'static Mutex<HashMap<String, ShellSession>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn read_slice(data: &[u8], ptr: i32, len: i32) -> String {
    let start = ptr as usize;
    let end = start.saturating_add(len as usize);
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

fn clamp_timeout_ms(value: i32) -> u64 {
    if value <= 0 {
        30_000
    } else {
        (value as u64).min(30_000)
    }
}

fn clamp_lifetime_ms(value: i32) -> u64 {
    if value <= 0 {
        600_000
    } else {
        (value as u64).min(600_000)
    }
}

fn clamp_output_bytes(value: i32) -> usize {
    if value <= 0 {
        65_536
    } else {
        (value as usize).min(65_536)
    }
}

fn duration_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}
