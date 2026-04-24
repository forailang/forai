//! Platform abstraction for I/O operations.
//!
//! Native and WASM targets provide different implementations.

pub trait Platform {
    fn print(&mut self, msg: &str);
    fn read_file(&self, path: &str) -> Result<String, String>;
    fn write_file(&self, path: &str, content: &str) -> Result<(), String>;
    fn now_ms(&self) -> f64;
    fn random(&self) -> f64;
    fn sleep_ms(&self, ms: f64);
}

/// Native platform using std library (not available on WASM).
#[cfg(not(target_arch = "wasm32"))]
pub struct NativePlatform;

#[cfg(not(target_arch = "wasm32"))]
impl Platform for NativePlatform {
    fn print(&mut self, msg: &str) {
        println!("{}", msg);
    }

    fn read_file(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        std::fs::write(path, content).map_err(|e| e.to_string())
    }

    fn now_ms(&self) -> f64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as f64
    }

    fn random(&self) -> f64 {
        use std::cell::Cell;
        thread_local! {
            static STATE: Cell<u64> = Cell::new(0x12345678_9abcdef0);
        }
        STATE.with(|s| {
            let mut x = s.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.set(x);
            (x as f64) / (u64::MAX as f64)
        })
    }

    fn sleep_ms(&self, ms: f64) {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    }
}
