//! Host-side stdout/stderr sinks for the wasm runner.
//!
//! Host imports (`env.print`, router logs, return-value printing) all route
//! here instead of calling `println!`/`eprintln!` directly. The default sink
//! is real stdout/stderr. Tests can swap in a capturing sink via
//! [`CaptureGuard`] to collect output for assertions.

use std::cell::RefCell;
use std::io::Write;
#[cfg(test)]
use std::sync::{Arc, Mutex};

thread_local! {
    static STDOUT_SINK: RefCell<Sink> = RefCell::new(Sink::Stdout);
    static STDERR_SINK: RefCell<Sink> = RefCell::new(Sink::Stderr);
}

enum Sink {
    Stdout,
    Stderr,
    #[cfg(test)]
    Buffer(Arc<Mutex<Vec<u8>>>),
}

impl Sink {
    fn write_line(&mut self, s: &str) {
        match self {
            Sink::Stdout => {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{}", s);
            }
            Sink::Stderr => {
                let mut err = std::io::stderr().lock();
                let _ = writeln!(err, "{}", s);
            }
            #[cfg(test)]
            Sink::Buffer(buf) => {
                if let Ok(mut b) = buf.lock() {
                    b.extend_from_slice(s.as_bytes());
                    b.push(b'\n');
                }
            }
        }
    }
}

/// Write a line to the active host stdout sink.
pub(crate) fn stdout_line(s: &str) {
    STDOUT_SINK.with(|sink| sink.borrow_mut().write_line(s));
}

/// Write a line to the active host stderr sink.
pub(crate) fn stderr_line(s: &str) {
    STDERR_SINK.with(|sink| sink.borrow_mut().write_line(s));
}

/// RAII capture guard. While held, host stdout/stderr writes are appended to
/// internal buffers. Dropping the guard restores the previous sinks.
#[cfg(test)]
pub struct CaptureGuard {
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    prev_stdout: Option<Sink>,
    prev_stderr: Option<Sink>,
}

#[cfg(test)]
impl CaptureGuard {
    pub fn new() -> Self {
        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let prev_stdout = STDOUT_SINK.with(|s| {
            Some(std::mem::replace(
                &mut *s.borrow_mut(),
                Sink::Buffer(stdout_buf.clone()),
            ))
        });
        let prev_stderr = STDERR_SINK.with(|s| {
            Some(std::mem::replace(
                &mut *s.borrow_mut(),
                Sink::Buffer(stderr_buf.clone()),
            ))
        });
        Self {
            stdout_buf,
            stderr_buf,
            prev_stdout,
            prev_stderr,
        }
    }

    /// Snapshot of captured stdout so far (UTF-8 lossy).
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout_buf.lock().unwrap()).into_owned()
    }

    /// Snapshot of captured stderr so far (UTF-8 lossy).
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr_buf.lock().unwrap()).into_owned()
    }
}

#[cfg(test)]
impl Drop for CaptureGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.prev_stdout.take() {
            STDOUT_SINK.with(|s| *s.borrow_mut() = prev);
        }
        if let Some(prev) = self.prev_stderr.take() {
            STDERR_SINK.with(|s| *s.borrow_mut() = prev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_guard_captures_stdout_and_stderr() {
        let guard = CaptureGuard::new();
        stdout_line("hello");
        stderr_line("oops");
        assert_eq!(guard.stdout(), "hello\n");
        assert_eq!(guard.stderr(), "oops\n");
    }

    #[test]
    fn capture_guard_restores_previous_sink_on_drop() {
        {
            let _outer = CaptureGuard::new();
            {
                let inner = CaptureGuard::new();
                stdout_line("inner");
                assert_eq!(inner.stdout(), "inner\n");
            }
            // After inner drops, the outer guard should be active again.
            stdout_line("outer");
            assert_eq!(_outer.stdout(), "outer\n");
        }
    }
}
