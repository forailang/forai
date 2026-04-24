//! Tracks opaque C pointers (void*) as integer handles.
//!
//! FAI `Ptr` values are NaN-boxed integers internally. This tracker maps
//! handle IDs to raw `*mut c_void` pointers, preventing use-after-free
//! by checking validity on access.

use std::collections::HashMap;
use std::os::raw::c_void;

/// Maps integer handles to raw C pointers.
///
/// Each tracked pointer gets a unique monotonic handle ID.
/// Handles can be released (invalidated) to catch use-after-free.
pub struct PtrTracker {
    next_handle: u32,
    pointers: HashMap<u32, *mut c_void>,
}

// Raw pointers are not Send/Sync by default, but PtrTracker is only used
// within the single-threaded wasmtime host context.
unsafe impl Send for PtrTracker {}

impl PtrTracker {
    pub fn new() -> Self {
        Self {
            next_handle: 1, // 0 reserved for null
            pointers: HashMap::new(),
        }
    }

    /// Track a new pointer and return its handle ID.
    pub fn track(&mut self, ptr: *mut c_void) -> u32 {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.pointers.insert(handle, ptr);
        handle
    }

    /// Get the raw pointer for a handle, if still valid.
    pub fn get(&self, handle: u32) -> Option<*mut c_void> {
        self.pointers.get(&handle).copied()
    }

    /// Release a handle, invalidating future access.
    /// Returns the raw pointer if the handle was valid.
    pub fn release(&mut self, handle: u32) -> Option<*mut c_void> {
        self.pointers.remove(&handle)
    }

    /// Check if a handle is still valid.
    pub fn is_valid(&self, handle: u32) -> bool {
        self.pointers.contains_key(&handle)
    }

    /// Number of currently tracked pointers.
    pub fn count(&self) -> usize {
        self.pointers.len()
    }
}

impl Default for PtrTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_track_and_get() {
        let mut tracker = PtrTracker::new();
        let fake_ptr = 0xDEADBEEF as *mut c_void;
        let handle = tracker.track(fake_ptr);
        assert_eq!(tracker.get(handle), Some(fake_ptr));
    }

    #[test]
    fn test_release_invalidates() {
        let mut tracker = PtrTracker::new();
        let fake_ptr = 0xCAFEBABE as *mut c_void;
        let handle = tracker.track(fake_ptr);
        assert!(tracker.is_valid(handle));

        let released = tracker.release(handle);
        assert_eq!(released, Some(fake_ptr));
        assert!(!tracker.is_valid(handle));
        assert_eq!(tracker.get(handle), None);
    }

    #[test]
    fn test_handles_are_unique() {
        let mut tracker = PtrTracker::new();
        let h1 = tracker.track(0x1 as *mut c_void);
        let h2 = tracker.track(0x2 as *mut c_void);
        let h3 = tracker.track(0x3 as *mut c_void);
        assert_ne!(h1, h2);
        assert_ne!(h2, h3);
        assert_eq!(tracker.count(), 3);
    }

    #[test]
    fn test_invalid_handle_returns_none() {
        let tracker = PtrTracker::new();
        assert_eq!(tracker.get(999), None);
        assert!(!tracker.is_valid(0));
    }
}
