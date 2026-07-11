//! Incremental test cache (plan 135).
//!
//! Records, per source file, a content hash and whether that file's
//! co-located tests last passed. `fai run` (the "start" pipeline) uses it to
//! rerun only the suites in files that changed since the last green run — and
//! to run nothing at all when `fai test` just passed. `fai test` never
//! consults the cache; it runs everything and rewrites the cache as all-pass.
//!
//! Soundness rests on co-location (plan 135 rule 1): a `def`'s test lives in
//! the `def`'s file, so "file changed → its suites rerun" actually re-exercises
//! the changed code. Cross-file integration effects are explicitly out of scope
//! for the `start` guide (rule 4) — `fai test` is the truth.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CACHE_REL: &str = ".fai/test-cache.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileEntry {
    pub hash: String,
    pub last_pass: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TestCache {
    /// Toolchain identity; a mismatch drops the whole cache.
    pub fai_version: String,
    /// Canonical file path → last-seen hash + whether its tests passed then.
    pub files: std::collections::BTreeMap<String, FileEntry>,
}

/// Stable, dependency-free content hash (FNV-1a 64) as hex.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:016x}", h)
}

pub fn hash_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| hash_bytes(&b))
}

/// Toolchain identity: crate version + the fai executable's mtime, so a
/// rebuilt/reinstalled binary invalidates the cache — a compiler/runtime
/// change can flip test outcomes with no source change (plan 135). For a
/// downloaded release the mtime is stable, so it rarely invalidates.
pub fn toolchain_version() -> String {
    let ver = env!("CARGO_PKG_VERSION");
    let mtime = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(&p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}+{}", ver, mtime)
}

/// Canonicalize for stable cache keys; fall back to the raw path.
pub fn canon(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

fn cache_path(project_root: &Path) -> PathBuf {
    project_root.join(CACHE_REL)
}

/// Load the cache for a project root, or a fresh one stamped with the current
/// toolchain version if the file is missing / unparsable / from another
/// toolchain (in which case every file reads as dirty).
pub fn load(project_root: &Path) -> TestCache {
    let path = cache_path(project_root);
    let loaded: Option<TestCache> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    match loaded {
        Some(c) if c.fai_version == toolchain_version() => c,
        _ => TestCache {
            fai_version: toolchain_version(),
            files: Default::default(),
        },
    }
}

pub fn save(project_root: &Path, cache: &TestCache) {
    let path = cache_path(project_root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(cache) {
        let _ = std::fs::write(&path, json);
    }
}

/// True when `file` differs from its cached passing hash (or has none). An
/// unreadable file reads as dirty (run it, don't silently skip).
pub fn is_dirty(cache: &TestCache, file: &str) -> bool {
    let key = canon(file);
    let cur = match hash_file(Path::new(&key)) {
        Some(h) => h,
        None => return true,
    };
    match cache.files.get(&key) {
        Some(e) => !(e.last_pass && e.hash == cur),
        None => true,
    }
}

/// Record these files as passing at their current content hash.
pub fn mark_passed(cache: &mut TestCache, files: &HashSet<String>) {
    for f in files {
        let key = canon(f);
        if let Some(h) = hash_file(Path::new(&key)) {
            cache.files.insert(
                key,
                FileEntry {
                    hash: h,
                    last_pass: true,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_distinct() {
        assert_eq!(hash_bytes(b"hello"), hash_bytes(b"hello"));
        assert_ne!(hash_bytes(b"hello"), hash_bytes(b"world"));
    }

    #[test]
    fn missing_entry_is_dirty() {
        let cache = TestCache::default();
        // A path that doesn't exist reads as dirty (unreadable → run it).
        assert!(is_dirty(&cache, "/nonexistent/does-not-exist.fai"));
    }
}
