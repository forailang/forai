//! Wasm debug metadata emission (plan 116 phase 1).
//!
//! Two sections, appended to every produced module:
//!
//! 1. The standard wasm `name` section (function names). Wasmtime
//!    backtraces and browser DevTools pick it up automatically, so a
//!    trap shows `Forsqlite.migrate` instead of `wasm-function[133]`.
//! 2. A `fai-dbg` custom section: JSON mapping function index →
//!    `{ name, file, line }` (declaration site), plus optional
//!    ownership-site metadata. The native runner reads it to decorate
//!    backtrace frames as `name (file:line)`; `fai build` extracts it
//!    to `<out>.dbg.json` for external tools.
//!
//! Both are emitted unconditionally — we are in the dogfooding era of
//! a brand-new language and bytes are cheaper than debugging hours.
//! Custom sections are skippable by spec, so runtimes that don't know
//! `fai-dbg` ignore it.

use wasm_encoder::{CustomSection, Module, NameMap, NameSection};

/// Name of the custom section carrying the function debug table.
pub const DBG_SECTION_NAME: &str = "fai-dbg";

/// Debug record for one wasm function.
pub struct FnDebugEntry {
    /// Wasm function index (imports included).
    pub index: u32,
    /// Display name: import name, `rt_*` helper name, qualified forai
    /// name, `name#resume` for async resume fns, `<closure@l:c>`.
    pub name: String,
    /// Source file of the declaration, when known.
    pub file: Option<String>,
    /// 1-based declaration line; 0 = unknown/synthesised.
    pub line: u32,
}

impl FnDebugEntry {
    /// Entry with no source location (imports, runtime helpers,
    /// synthesised functions).
    pub fn unlocated(index: u32, name: impl Into<String>) -> Self {
        Self {
            index,
            name: name.into(),
            file: None,
            line: 0,
        }
    }
}

/// Debug record for one helper-level ownership instrumentation site.
///
/// The event ABI carries only a compact `site` integer; this side table
/// resolves that integer to a stable, human-readable operation label.
#[derive(Debug, Clone)]
pub struct OwnershipSiteDebugEntry {
    /// Dense nonzero site id emitted as the second argument to
    /// `__fai_ownership_event`.
    pub id: u32,
    /// Ownership operation name (`retain`, `cleanup`, ...). Duplicates
    /// the op argument intentionally so a site can be understood from
    /// the side table alone.
    pub op: &'static str,
    /// Broad codegen/helper area that emitted the event.
    pub helper: &'static str,
    /// Compact operation reason such as `store owned slot`.
    pub reason: &'static str,
    /// Source file for the enclosing forai function, when known.
    pub file: Option<String>,
    /// Best available line. Initially this is often the enclosing
    /// function declaration; later passes can thread statement spans.
    pub line: u32,
}

/// Module-level debug metadata carried alongside the function table.
/// Compile-time layout constants the host needs for post-mortem heap
/// stats (plan 116 phase 2) — they exist nowhere at runtime (baked
/// into instructions), so the side table is the only channel.
#[derive(Default)]
pub struct DbgMeta {
    /// Start of the size-bucketed free-list head array (one i32 head
    /// per bucket). `None` for modules without the bucketed allocator.
    pub bucket_base: Option<u32>,
    /// Number of free-list buckets at `bucket_base`.
    pub bucket_count: u32,
    /// Helper-level ownership instrumentation sites.
    pub ownership_sites: Vec<OwnershipSiteDebugEntry>,
}

/// Append the `name` section and the `fai-dbg` custom section to a
/// module under construction. Call after all standard sections (the
/// name section conventionally trails the data section). Entries must
/// be in ascending function-index order.
pub fn append_debug_sections(module: &mut Module, entries: &[FnDebugEntry], meta: &DbgMeta) {
    let mut map = NameMap::new();
    for e in entries {
        map.append(e.index, &e.name);
    }
    let mut names = NameSection::new();
    names.functions(&map);
    module.section(&names);

    module.section(&CustomSection {
        name: DBG_SECTION_NAME.into(),
        data: render_dbg_json(entries, meta).into_bytes().into(),
    });
}

/// Render the `fai-dbg` JSON payload. Hand-rolled writer — the only
/// dynamic strings are function names and file paths, escaped below;
/// this crate has no serde dependency and doesn't need one for this.
fn render_dbg_json(entries: &[FnDebugEntry], meta: &DbgMeta) -> String {
    let mut out = String::from("{\"version\":1,");
    if let Some(bucket_base) = meta.bucket_base {
        out.push_str(&format!(
            "\"heap\":{{\"bucket_base\":{},\"bucket_count\":{}}},",
            bucket_base, meta.bucket_count,
        ));
    }
    if !meta.ownership_sites.is_empty() {
        out.push_str("\"ownership_sites\":[");
        for (i, site) in meta.ownership_sites.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"id\":{},\"op\":\"{}\",\"helper\":\"{}\",\"reason\":\"{}\"",
                site.id, site.op, site.helper, site.reason
            ));
            if let Some(file) = &site.file {
                out.push_str(",\"file\":\"");
                escape_json_into(&mut out, file);
                out.push('"');
            }
            if site.line > 0 {
                out.push_str(&format!(",\"line\":{}", site.line));
            }
            out.push('}');
        }
        out.push_str("],");
    }
    out.push_str("\"functions\":[");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"index\":{},\"name\":\"", e.index));
        escape_json_into(&mut out, &e.name);
        out.push('"');
        if let Some(file) = &e.file {
            out.push_str(",\"file\":\"");
            escape_json_into(&mut out, file);
            out.push('"');
        }
        if e.line > 0 {
            out.push_str(&format!(",\"line\":{}", e.line));
        }
        out.push('}');
    }
    out.push_str("]}");
    out
}

fn escape_json_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escapes_and_omits_unknown_fields() {
        let entries = vec![
            FnDebugEntry::unlocated(0, "print"),
            FnDebugEntry {
                index: 7,
                name: "data.\"q\".fn".to_string(),
                file: Some("src/a.fai".to_string()),
                line: 12,
            },
        ];
        let json = render_dbg_json(&entries, &DbgMeta::default());
        assert_eq!(
            json,
            "{\"version\":1,\"functions\":[{\"index\":0,\"name\":\"print\"},{\"index\":7,\"name\":\"data.\\\"q\\\".fn\",\"file\":\"src/a.fai\",\"line\":12}]}"
        );
    }

    #[test]
    fn json_includes_heap_meta_when_present() {
        let json = render_dbg_json(
            &[],
            &DbgMeta {
                bucket_base: Some(4096),
                bucket_count: 1024,
                ownership_sites: Vec::new(),
            },
        );
        assert_eq!(
            json,
            "{\"version\":1,\"heap\":{\"bucket_base\":4096,\"bucket_count\":1024},\"functions\":[]}"
        );
    }

    #[test]
    fn json_includes_ownership_sites_when_present() {
        let json = render_dbg_json(
            &[],
            &DbgMeta {
                bucket_base: None,
                bucket_count: 0,
                ownership_sites: vec![OwnershipSiteDebugEntry {
                    id: 1,
                    op: "retain",
                    helper: "direct",
                    reason: "retain borrowed value",
                    file: Some("src/main.fai".to_string()),
                    line: 42,
                }],
            },
        );
        assert_eq!(
            json,
            "{\"version\":1,\"ownership_sites\":[{\"id\":1,\"op\":\"retain\",\"helper\":\"direct\",\"reason\":\"retain borrowed value\",\"file\":\"src/main.fai\",\"line\":42}],\"functions\":[]}"
        );
    }
}
