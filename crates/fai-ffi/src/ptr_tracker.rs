//! Tracks opaque C pointers (void*) as integer handles.
//!
//! FAI `Ptr` values are NaN-boxed integers internally. This tracker maps
//! handle IDs to raw `*mut c_void` pointers, preventing use-after-free
//! by checking validity on access.

use std::collections::HashMap;
use std::os::raw::c_void;

/// Maps integer handles to raw C pointers.
///
/// Each distinct live address gets a stable handle ID. `track` deduplicates by
/// address: re-tracking an address already in the map returns its existing
/// handle instead of minting a new one. This matters because the runtime never
/// releases handles for opaque C objects (there is no generic signal for "this
/// extern call freed its pointer"), and a library like sqlite recycles freed
/// memory (statements, DB handles) at the same addresses — so without dedup the
/// map would grow by a fresh entry on every `prepare`/`open`, an unbounded
/// host-side leak. Dedup bounds it to the number of distinct addresses ever
/// live at once. It is also strictly safer than the old mint-always behavior:
/// an address is only reused after its previous object was freed, so mapping
/// the recycled address back to one stable handle can never alias two live
/// objects (the alternative left the old handle dangling at freed memory).
pub struct PtrTracker {
    next_handle: u32,
    pointers: HashMap<u32, *mut c_void>,
    /// Reverse index (address → handle) backing `track`'s dedup.
    by_addr: HashMap<usize, u32>,
}

// Raw pointers are not Send/Sync by default, but PtrTracker is only used
// within the single-threaded wasmtime host context.
unsafe impl Send for PtrTracker {}

impl PtrTracker {
    pub fn new() -> Self {
        Self {
            next_handle: 1, // 0 reserved for null
            pointers: HashMap::new(),
            by_addr: HashMap::new(),
        }
    }

    /// Track a pointer and return its handle ID. Re-tracking an address already
    /// in the map returns its existing handle (see the type doc) rather than
    /// minting a new one, keeping the map bounded to distinct live addresses.
    pub fn track(&mut self, ptr: *mut c_void) -> u32 {
        let addr = ptr as usize;
        if let Some(&handle) = self.by_addr.get(&addr) {
            // Same address, recycled by the allocator for a new object: reuse
            // the stable handle and refresh its mapping.
            self.pointers.insert(handle, ptr);
            return handle;
        }
        let handle = self.next_handle;
        self.next_handle += 1;
        self.pointers.insert(handle, ptr);
        self.by_addr.insert(addr, handle);
        handle
    }

    /// Get the raw pointer for a handle, if still valid.
    pub fn get(&self, handle: u32) -> Option<*mut c_void> {
        self.pointers.get(&handle).copied()
    }

    /// Release a handle, invalidating future access.
    /// Returns the raw pointer if the handle was valid.
    pub fn release(&mut self, handle: u32) -> Option<*mut c_void> {
        let ptr = self.pointers.remove(&handle);
        if let Some(p) = ptr {
            // Drop the reverse entry too, but only if it still points at this
            // handle — a later re-track of the same address may have rebound it.
            let addr = p as usize;
            if self.by_addr.get(&addr) == Some(&handle) {
                self.by_addr.remove(&addr);
            }
        }
        ptr
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
