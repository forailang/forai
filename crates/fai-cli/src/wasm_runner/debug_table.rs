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
struct OnDiskOwnershipSite {
    id: u32,
    op: String,
    helper: String,
    reason: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: u32,
}

#[derive(serde::Deserialize)]
struct OnDisk {
    functions: Vec<OnDiskFn>,
    #[serde(default)]
    heap: Option<OnDiskHeap>,
    #[serde(default)]
    ownership_sites: Vec<OnDiskOwnershipSite>,
}

/// Debug info for one wasm function.
pub(crate) struct DbgFn {
    pub name: String,
    pub file: Option<String>,
    pub line: u32,
}

/// Debug info for one helper-level ownership instrumentation site.
pub(crate) struct DbgOwnershipSite {
    pub op: String,
    pub helper: String,
    pub reason: String,
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
    ownership_sites: HashMap<u32, DbgOwnershipSite>,
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
                        table.heap_buckets = on_disk.heap.map(|h| (h.bucket_base, h.bucket_count));
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
                        table.ownership_sites = on_disk
                            .ownership_sites
                            .into_iter()
                            .map(|s| {
                                (
                                    s.id,
                                    DbgOwnershipSite {
                                        op: s.op,
                                        helper: s.helper,
                                        reason: s.reason,
                                        file: s.file,
                                        line: s.line,
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

    /// Resolved name of a wasm function index, when the `fai-dbg`
    /// table knows it. Used by the leak ledger to attribute an
    /// allocation backtrace to the forai function that made it.
    pub(crate) fn func_name(&self, index: u32) -> Option<&str> {
        self.lookup(index).map(|f| f.name.as_str())
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

    /// Human-readable label for an ownership instrumentation site.
    pub(crate) fn ownership_site_label(&self, site_id: u32) -> String {
        if site_id == 0 {
            return "unknown ownership site".to_string();
        }
        match self.ownership_sites.get(&site_id) {
            Some(site) => {
                let loc = match (&site.file, site.line) {
                    (Some(file), line) if line > 0 => format!(" ({}:{})", file, line),
                    (None, line) if line > 0 => format!(" (line {})", line),
                    _ => String::new(),
                };
                format!("{}:{}:{}{}", site.helper, site.op, site.reason, loc)
            }
            None => format!("ownership site {}", site_id),
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

    fn encode_u32_leb(mut value: u32, out: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn wasm_with_fai_dbg(json: &str) -> Vec<u8> {
        let mut wasm = vec![
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
        ];
        let mut payload = Vec::new();
        encode_u32_leb("fai-dbg".len() as u32, &mut payload);
        payload.extend_from_slice(b"fai-dbg");
        payload.extend_from_slice(json.as_bytes());

        wasm.push(0); // custom section
        encode_u32_leb(payload.len() as u32, &mut wasm);
        wasm.extend(payload);
        wasm
    }

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

    #[test]
    fn ownership_site_label_falls_back_for_unknown_site() {
        let table = DbgTable::default();
        assert_eq!(table.ownership_site_label(0), "unknown ownership site");
        assert_eq!(table.ownership_site_label(7), "ownership site 7");
    }

    #[test]
    fn ownership_site_label_resolves_known_site() {
        let wasm = wasm_with_fai_dbg(
            "{\"version\":1,\"ownership_sites\":[{\"id\":3,\"op\":\"store\",\"helper\":\"direct\",\"reason\":\"store owned value\",\"file\":\"app.fai\",\"line\":12}],\"functions\":[]}",
        );
        let table = DbgTable::from_wasm(&wasm);

        assert_eq!(
            table.ownership_site_label(3),
            "direct:store:store owned value (app.fai:12)"
        );
    }
}
