//! Parse the `fai-dbg` custom section (plan 116) and decorate trap
//! backtraces with it.
//!
//! The codegen embeds `{ index, name, file, line }` per wasm function
//! (see `fai-codegen-wasm/src/debug_info.rs`). On a trap, the runner
//! downcasts wasmtime's error to a [`wasmtime::WasmBacktrace`] and
//! renders each frame as `Forsqlite.migrate (src/migrate.fai:16)`
//! instead of `wasm-function[133]`. A guest-supplied trap reason (from
//! `__fai_set_trap_msg` / `__fai_trap_report`) leads the report.

use std::collections::HashMap;

#[derive(serde::Deserialize)]
struct OnDiskFn {
    index: u32,
    name: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: u32,
}

#[derive(serde::Deserialize)]
struct OnDiskHeap {
    bucket_base: u32,
    bucket_count: u32,
}

#[derive(serde::Deserialize)]
struct OnDisk {
    functions: Vec<OnDiskFn>,
    #[serde(default)]
    heap: Option<OnDiskHeap>,
}

/// Debug info for one wasm function.
pub(crate) struct DbgFn {
    pub name: String,
    pub file: Option<String>,
    pub line: u32,
}

/// Function-index → debug-info table parsed from a module's `fai-dbg`
/// custom section. Empty when the module carries no section (e.g. a
/// pre-plan-116 `.wasm`), in which case decoration degrades to the
/// names wasmtime itself resolves from the `name` section.
#[derive(Default)]
pub(crate) struct DbgTable {
    functions: HashMap<u32, DbgFn>,
    /// Allocator free-list bucket region `(base, bucket_count)`, from
    /// the `fai-dbg` heap metadata. Feeds post-mortem heap stats.
    pub(crate) heap_buckets: Option<(u32, u32)>,
    /// Indirect-function-table slots → wasm function index, parsed
    /// from the module's element section. Maps a task record's resume
    /// slot to a nameable function for the post-mortem task dump.
    table_map: Vec<u32>,
}

impl DbgTable {
    /// Parse the `fai-dbg` custom section (and the element section,
    /// for table-slot → function mapping) out of a wasm binary.
    /// Missing or malformed sections yield an empty table — debug
    /// metadata must never break a run.
    pub(crate) fn from_wasm(wasm: &[u8]) -> DbgTable {
        use wasmparser::{ElementItems, Parser, Payload};
        let mut table = DbgTable::default();
        for payload in Parser::new(0).parse_all(wasm) {
            match payload {
                Ok(Payload::CustomSection(reader)) if reader.name() == "fai-dbg" => {
                    if let Ok(on_disk) = serde_json::from_slice::<OnDisk>(reader.data()) {
                        table.heap_buckets = on_disk
                            .heap
                            .map(|h| (h.bucket_base, h.bucket_count));
                        table.functions = on_disk
                            .functions
                            .into_iter()
                            .map(|f| {
                                (
                                    f.index,
                                    DbgFn {
                                        name: f.name,
                                        file: f.file,
                                        line: f.line,
                                    },
                                )
                            })
                            .collect();
                    }
                }
                Ok(Payload::ElementSection(reader)) => {
                    // Both build paths emit a single active segment for
                    // table 0 starting at offset 0; collect its function
                    // indices in slot order.
                    for elem in reader.into_iter().flatten() {
                        if let ElementItems::Functions(fns) = elem.items {
                            if table.table_map.is_empty() {
                                table.table_map = fns.into_iter().flatten().collect();
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        table
    }

    fn lookup(&self, index: u32) -> Option<&DbgFn> {
        self.functions.get(&index)
    }

    /// Name of the function installed in indirect-table slot `slot`
    /// (e.g. a task record's resume fn). Falls back to the raw slot.
    pub(crate) fn table_slot_name(&self, slot: u32) -> String {
        match self
            .table_map
            .get(slot as usize)
            .and_then(|idx| self.lookup(*idx))
        {
            Some(f) => f.name.clone(),
            None => format!("<table slot {}>", slot),
        }
    }

    /// Render one backtrace frame: `name (file:line)` with whatever
    /// parts are known. Falls back to the wasmtime-resolved name (from
    /// the `name` section), then to the bare index.
    fn render_frame(&self, frame: &wasmtime::FrameInfo) -> String {
        let idx = frame.func_index();
        match self.lookup(idx) {
            Some(f) => match (&f.file, f.line) {
                (Some(file), l) if l > 0 => format!("{} ({}:{})", f.name, file, l),
                (None, l) if l > 0 => format!("{} (line {})", f.name, l),
                _ => f.name.clone(),
            },
            None => frame
                .func_name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("wasm-function[{}]", idx)),
        }
    }

    /// Format a trapped execution error into the plan-116 trap report:
    ///
    /// ```text
    /// trap: over-release (rc -1) of String "id" at 0x3fa38
    /// wasm backtrace:
    ///   0: rt_release
    ///   1: Forsqlite.migrate (src/migrate.fai:16)
    ///   2: main (main.fai:3)
    /// ```
    ///
    /// The reason line prefers the guest-stashed message (from
    /// `__fai_trap_report` / `__fai_set_trap_msg`); otherwise it's
    /// wasmtime's own trap description. Pass the result of
    /// `host::take_trap_msg()` as `trap_msg` — taking it here would
    /// hide it from callers that need the raw message (test runner).
    pub(crate) fn render_trap(
        &self,
        context: &str,
        e: &wasmtime::Error,
        trap_msg: Option<String>,
    ) -> String {
        let reason = match (&trap_msg, e.downcast_ref::<wasmtime::Trap>()) {
            (Some(msg), _) => msg.clone(),
            (None, Some(trap)) => trap.to_string(),
            (None, None) => format!("{:#}", e),
        };
        let mut out = format!("{}: {}", context, reason);
        if let Some(bt) = e.downcast_ref::<wasmtime::WasmBacktrace>() {
            if !bt.frames().is_empty() {
                out.push_str("\nwasm backtrace:");
                for (i, frame) in bt.frames().iter().enumerate() {
                    out.push_str(&format!("\n  {}: {}", i, self.render_frame(frame)));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_section_yields_empty_table() {
        let wasm = vec![
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
        ];
        let table = DbgTable::from_wasm(&wasm);
        assert!(table.functions.is_empty());
    }

    #[test]
    fn garbage_bytes_yield_empty_table() {
        let table = DbgTable::from_wasm(b"not a wasm module");
        assert!(table.functions.is_empty());
    }
}
