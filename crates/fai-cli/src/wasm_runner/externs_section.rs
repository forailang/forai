//! Round-trip FFI extern metadata through a wasm custom section so a
//! prebuilt `.wasm` carries everything the host needs to dispatch
//! `call_ffi` — without having the original `.fai` source available.
//!
//! `step_build` calls [`embed_externs`] before writing the wasm to
//! disk; `run_wasm` calls [`extract_externs`] before constructing the
//! [`ExternGuard`]. The on-disk shape uses string-tagged FfiType so we
//! don't have to derive serde on the fai-ffi crate.
//!
//! Custom sections are part of the wasm spec: any byte sequence the
//! runtime must skip and the spec preserves through tooling. Section
//! id `0`, name `"fai-externs"`. The payload is JSON for inspectability;
//! the size overhead is negligible (typical projects have a handful
//! of externs at most).
//!
//! When no externs are present the build skips the section entirely so
//! the produced wasm is byte-identical to the pre-feature output.

use super::host::util::ExternInfo;
use fai_ffi::FfiType;

const SECTION_NAME: &str = "fai-externs";

#[derive(serde::Serialize, serde::Deserialize)]
struct OnDisk {
    library: String,
    function: String,
    param_types: Vec<String>,
    return_type: String,
}

fn ffi_type_to_str(t: &FfiType) -> &'static str {
    match t {
        FfiType::Int => "Int",
        FfiType::Double => "Double",
        FfiType::String => "String",
        FfiType::Bool => "Bool",
        FfiType::Pointer => "Pointer",
        FfiType::Void => "Void",
        FfiType::OutPtr => "OutPtr",
    }
}

fn ffi_type_from_str(s: &str) -> Option<FfiType> {
    match s {
        "Int" => Some(FfiType::Int),
        "Double" => Some(FfiType::Double),
        "String" => Some(FfiType::String),
        "Bool" => Some(FfiType::Bool),
        "Pointer" => Some(FfiType::Pointer),
        "Void" => Some(FfiType::Void),
        "OutPtr" => Some(FfiType::OutPtr),
        _ => None,
    }
}

/// Append a `u32` in unsigned LEB128 form to `out`. Wasm sizes and
/// name-length fields are LEB128, so we hand-roll the encoder rather
/// than pull in a writer crate just for two call sites.
fn write_uleb128(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        byte |= 0x80;
        out.push(byte);
    }
}

/// Append a `fai-externs` custom section to a wasm binary. Skipped
/// when there are no externs to preserve byte-identity for projects
/// without `extern` blocks.
pub fn embed_externs(wasm: &mut Vec<u8>, externs: &[ExternInfo]) {
    if externs.is_empty() {
        return;
    }
    let on_disk: Vec<OnDisk> = externs
        .iter()
        .map(|e| OnDisk {
            library: e.library.clone(),
            function: e.function.clone(),
            param_types: e
                .param_types
                .iter()
                .map(|t| ffi_type_to_str(t).to_string())
                .collect(),
            return_type: ffi_type_to_str(&e.return_type).to_string(),
        })
        .collect();
    let payload = match serde_json::to_vec(&on_disk) {
        Ok(p) => p,
        Err(_) => return,
    };

    // Section content: [name_len:uleb128][name][payload]
    let mut content = Vec::new();
    write_uleb128(&mut content, SECTION_NAME.len() as u32);
    content.extend_from_slice(SECTION_NAME.as_bytes());
    content.extend_from_slice(&payload);

    // Section frame: [id=0][size:uleb128][content]
    wasm.push(0);
    write_uleb128(wasm, content.len() as u32);
    wasm.extend_from_slice(&content);
}

/// Read an embedded `fai-externs` section back into a list of
/// `ExternInfo`. Returns an empty vec when the section is missing,
/// malformed, or references unknown FfiType strings — host dispatch
/// silently produces VAL_NULL in that case rather than crashing.
pub fn extract_externs(wasm: &[u8]) -> Vec<ExternInfo> {
    use wasmparser::{Parser, Payload};
    let parser = Parser::new(0);
    for payload in parser.parse_all(wasm) {
        if let Ok(Payload::CustomSection(reader)) = payload {
            if reader.name() == SECTION_NAME {
                if let Ok(on_disk) = serde_json::from_slice::<Vec<OnDisk>>(reader.data()) {
                    let mut out = Vec::with_capacity(on_disk.len());
                    for e in on_disk {
                        let Some(return_type) = ffi_type_from_str(&e.return_type) else {
                            return Vec::new();
                        };
                        let param_types: Option<Vec<FfiType>> =
                            e.param_types.iter().map(|s| ffi_type_from_str(s)).collect();
                        let Some(param_types) = param_types else {
                            return Vec::new();
                        };
                        out.push(ExternInfo {
                            library: e.library,
                            function: e.function,
                            param_types,
                            return_type,
                        });
                    }
                    return out;
                }
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but valid wasm module: magic + version + an empty
    /// type section. We embed our custom section at the end.
    fn empty_module() -> Vec<u8> {
        vec![
            0x00, 0x61, 0x73, 0x6d, // "\0asm" magic
            0x01, 0x00, 0x00, 0x00, // version 1
        ]
    }

    fn sample_externs() -> Vec<ExternInfo> {
        vec![
            ExternInfo {
                library: "sqlite3".into(),
                function: "sqlite3_open".into(),
                param_types: vec![FfiType::String, FfiType::OutPtr],
                return_type: FfiType::Int,
            },
            ExternInfo {
                library: "m".into(),
                function: "pow".into(),
                param_types: vec![FfiType::Double, FfiType::Double],
                return_type: FfiType::Double,
            },
        ]
    }

    #[test]
    fn round_trip_through_custom_section() {
        let mut wasm = empty_module();
        embed_externs(&mut wasm, &sample_externs());
        let got = extract_externs(&wasm);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].library, "sqlite3");
        assert_eq!(got[0].function, "sqlite3_open");
        assert_eq!(got[0].param_types, vec![FfiType::String, FfiType::OutPtr]);
        assert_eq!(got[0].return_type, FfiType::Int);
        assert_eq!(got[1].library, "m");
        assert_eq!(got[1].function, "pow");
        assert_eq!(got[1].return_type, FfiType::Double);
    }

    #[test]
    fn empty_externs_leave_wasm_byte_identical() {
        let original = empty_module();
        let mut wasm = original.clone();
        embed_externs(&mut wasm, &[]);
        assert_eq!(wasm, original);
    }

    #[test]
    fn extract_returns_empty_when_no_section() {
        let wasm = empty_module();
        assert!(extract_externs(&wasm).is_empty());
    }

    #[test]
    fn extract_returns_empty_for_non_wasm_bytes() {
        let bytes = b"not a wasm module";
        assert!(extract_externs(bytes).is_empty());
    }

    #[test]
    fn extract_returns_empty_when_section_payload_is_garbage() {
        // Manually craft a custom section with our name but a bad
        // JSON payload, ensure the extractor doesn't panic.
        let mut wasm = empty_module();
        let mut content = Vec::new();
        write_uleb128(&mut content, SECTION_NAME.len() as u32);
        content.extend_from_slice(SECTION_NAME.as_bytes());
        content.extend_from_slice(b"not json");
        wasm.push(0);
        write_uleb128(&mut wasm, content.len() as u32);
        wasm.extend_from_slice(&content);
        assert!(extract_externs(&wasm).is_empty());
    }

    #[test]
    fn write_uleb128_known_values() {
        let mut out = Vec::new();
        write_uleb128(&mut out, 0);
        assert_eq!(out, vec![0]);
        out.clear();
        write_uleb128(&mut out, 127);
        assert_eq!(out, vec![127]);
        out.clear();
        write_uleb128(&mut out, 128);
        assert_eq!(out, vec![0x80, 0x01]);
        out.clear();
        write_uleb128(&mut out, 624485);
        assert_eq!(out, vec![0xe5, 0x8e, 0x26]);
    }

    #[test]
    fn round_trip_preserves_all_ffi_type_variants() {
        let externs = vec![ExternInfo {
            library: "test".into(),
            function: "all_types".into(),
            param_types: vec![
                FfiType::Int,
                FfiType::Double,
                FfiType::String,
                FfiType::Bool,
                FfiType::Pointer,
                FfiType::OutPtr,
            ],
            return_type: FfiType::Void,
        }];
        let mut wasm = empty_module();
        embed_externs(&mut wasm, &externs);
        let got = extract_externs(&wasm);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].param_types.len(), 6);
        assert_eq!(got[0].param_types[0], FfiType::Int);
        assert_eq!(got[0].param_types[1], FfiType::Double);
        assert_eq!(got[0].param_types[2], FfiType::String);
        assert_eq!(got[0].param_types[3], FfiType::Bool);
        assert_eq!(got[0].param_types[4], FfiType::Pointer);
        assert_eq!(got[0].param_types[5], FfiType::OutPtr);
        assert_eq!(got[0].return_type, FfiType::Void);
    }
}
