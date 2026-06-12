//! I/O host imports: `print`, `read_file`, `write_file`, `set_html`,
//! `set_html_at`, `file_exists`, `log_*`, `path_*`, `html_escape`.

use wasmtime::*;

use super::super::heap::wasm_alloc_str;
use super::super::nan_box::VAL_NULL;
use super::super::output;

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.print(ptr, len) — write string to stdout
    linker
        .func_wrap(
            "env",
            "print",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let end = (ptr + len) as usize;
                if end <= data.len() {
                    let s =
                        std::str::from_utf8(&data[ptr as usize..end]).unwrap_or("<invalid utf8>");
                    output::stdout_line(s);
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.read_file(path_ptr, path_len, buf_ptr) -> content_len (or -1)
    linker
        .func_wrap(
            "env",
            "read_file",
            |mut caller: Caller<'_, ()>, path_ptr: i32, path_len: i32, buf_ptr: i32| -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let path = {
                    let data = mem.data(&caller);
                    let end = (path_ptr + path_len) as usize;
                    if end > data.len() {
                        return -1;
                    }
                    std::str::from_utf8(&data[path_ptr as usize..end])
                        .unwrap_or("")
                        .to_string()
                };
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        let bytes = content.as_bytes();
                        // Legacy scratch-buffer ABI: callers allocate a fixed
                        // 64 KiB buffer, which this copy used to overflow on
                        // larger files (silent heap corruption — the brain
                        // suite found it as free-list poisoning). The direct
                        // codegen now uses `file_read_str`; refuse oversized
                        // content here so any straggler caller gets a clean
                        // failure instead of a scribbled heap.
                        if bytes.len() > 65536 {
                            return -1;
                        }
                        let data = mem.data_mut(&mut caller);
                        let dst = buf_ptr as usize;
                        if dst + bytes.len() <= data.len() {
                            data[dst..dst + bytes.len()].copy_from_slice(bytes);
                            bytes.len() as i32
                        } else {
                            -1
                        }
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.file_read_str(path_ptr, path_len) -> i64 — NaN-boxed String with
    // the full file contents (host-allocated, any size), or VAL_NULL on
    // failure. Replaces the fixed-buffer `read_file` ABI above.
    linker
        .func_wrap(
            "env",
            "file_read_str",
            |mut caller: Caller<'_, ()>, path_ptr: i32, path_len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let path = {
                    let data = mem.data(&caller);
                    let end = (path_ptr + path_len) as usize;
                    if end > data.len() {
                        return VAL_NULL;
                    }
                    std::str::from_utf8(&data[path_ptr as usize..end])
                        .unwrap_or("")
                        .to_string()
                };
                match std::fs::read_to_string(&path) {
                    Ok(content) => wasm_alloc_str(&mut caller, &mem, &content),
                    Err(_) => VAL_NULL,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.file_exists(path_ptr, path_len) -> i32 (1 if path exists, 0 otherwise)
    linker
        .func_wrap(
            "env",
            "file_exists",
            |mut caller: Caller<'_, ()>, path_ptr: i32, path_len: i32| -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let end = (path_ptr + path_len) as usize;
                if end > data.len() {
                    return 0;
                }
                let path = std::str::from_utf8(&data[path_ptr as usize..end]).unwrap_or("");
                if std::path::Path::new(path).exists() {
                    1
                } else {
                    0
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.write_file(path_ptr, path_len, content_ptr, content_len) -> 1 success, 0 failure
    //
    // Returns a 0/1 flag matching forai's `Bool` convention so the
    // direct-path codegen can wrap the i32 via `RT_MAKE_BOOL`. The
    // older 0/-1 encoding survived from the bytecode era when this
    // result wasn't exposed to forai code.
    linker
        .func_wrap(
            "env",
            "write_file",
            |mut caller: Caller<'_, ()>,
             path_ptr: i32,
             path_len: i32,
             content_ptr: i32,
             content_len: i32|
             -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let path_end = (path_ptr + path_len) as usize;
                let content_end = (content_ptr + content_len) as usize;
                if path_end > data.len() || content_end > data.len() {
                    return 0;
                }
                let path = std::str::from_utf8(&data[path_ptr as usize..path_end])
                    .unwrap_or("")
                    .to_string();
                let content = std::str::from_utf8(&data[content_ptr as usize..content_end])
                    .unwrap_or("")
                    .to_string();
                match std::fs::write(&path, &content) {
                    Ok(_) => 1,
                    Err(_) => 0,
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.set_html(ptr, len) -> void
    linker
        .func_wrap(
            "env",
            "set_html",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let html =
                    String::from_utf8_lossy(&data[ptr as usize..(ptr + len) as usize]).into_owned();
                // In CLI/Wasmtime mode, just print the HTML
                output::stdout_line(&html);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.log_info / log_warn / log_error (ptr, len) -> void. Each prints
    // `[LEVEL] <msg>` through the shared stdout sink (capturable).
    for (name, prefix) in [
        ("log_info", "[INFO] "),
        ("log_warn", "[WARN] "),
        ("log_error", "[ERROR] "),
    ] {
        linker
            .func_wrap(
                "env",
                name,
                move |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
                    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                    let data = mem.data(&caller);
                    let end = (ptr + len) as usize;
                    if end > data.len() {
                        return;
                    }
                    let msg =
                        std::str::from_utf8(&data[ptr as usize..end]).unwrap_or("<invalid utf8>");
                    output::stdout_line(&format!("{}{}", prefix, msg));
                },
            )
            .map_err(|e| format!("linker error: {}", e))?;
    }

    // env.set_html_at(selector_ptr, selector_len, html_ptr, html_len) -> void
    linker
        .func_wrap(
            "env",
            "set_html_at",
            |mut caller: Caller<'_, ()>,
             _selector_ptr: i32,
             _selector_len: i32,
             html_ptr: i32,
             html_len: i32| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let html = String::from_utf8_lossy(
                    &data[html_ptr as usize..(html_ptr + html_len) as usize],
                )
                .into_owned();
                output::stdout_line(&html);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // std.path.* — pure string transforms backed by std::path::Path so the
    // wasm target behaves the same as the VM's native_path_* natives.
    linker
        .func_wrap(
            "env",
            "path_join",
            |mut caller: Caller<'_, ()>, l_ptr: i32, l_len: i32, r_ptr: i32, r_len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let (left, right) = {
                    let data = mem.data(&caller);
                    (
                        read_slice(data, l_ptr, l_len),
                        read_slice(data, r_ptr, r_len),
                    )
                };
                let joined = std::path::Path::new(&left)
                    .join(&right)
                    .to_string_lossy()
                    .into_owned();
                wasm_alloc_str(&mut caller, &mem, &joined)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "path_basename",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let path = {
                    let data = mem.data(&caller);
                    read_slice(data, ptr, len)
                };
                let basename = std::path::Path::new(&path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                wasm_alloc_str(&mut caller, &mem, &basename)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "path_dirname",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let path = {
                    let data = mem.data(&caller);
                    read_slice(data, ptr, len)
                };
                let dirname = std::path::Path::new(&path)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                wasm_alloc_str(&mut caller, &mem, &dirname)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    linker
        .func_wrap(
            "env",
            "path_extname",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let path = {
                    let data = mem.data(&caller);
                    read_slice(data, ptr, len)
                };
                let ext = std::path::Path::new(&path)
                    .extension()
                    .map(|s| format!(".{}", s.to_string_lossy()))
                    .unwrap_or_default();
                wasm_alloc_str(&mut caller, &mem, &ext)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // std.html.escape — five-character-class entity replacement. Matches
    // `native_html_escape` byte-for-byte.
    linker
        .func_wrap(
            "env",
            "html_escape",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let text = {
                    let data = mem.data(&caller);
                    read_slice(data, ptr, len)
                };
                let escaped = text
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&#39;");
                wasm_alloc_str(&mut caller, &mem, &escaped)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.file_list(ptr, len) -> i64 (Array<String> | null). Reads the
    // directory, collects entry file names, and allocates a NaN-boxed
    // Array<String> on the guest heap. Returns VAL_NULL on I/O error.
    linker
        .func_wrap(
            "env",
            "file_list",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let path = {
                    let data = mem.data(&caller);
                    read_slice(data, ptr, len)
                };
                match std::fs::read_dir(&path) {
                    Ok(entries) => {
                        let mut names: Vec<String> = Vec::new();
                        for e in entries.flatten() {
                            names.push(e.file_name().to_string_lossy().into_owned());
                        }
                        // Build via serde_json so build_value handles the
                        // Array<String> layout and heap bump consistently.
                        let json_arr = serde_json::Value::Array(
                            names.into_iter().map(serde_json::Value::String).collect(),
                        );
                        super::super::heap::build_value(&mut caller, &mem, &json_arr)
                    }
                    Err(_) => {
                        let json_arr = serde_json::Value::Array(Vec::new());
                        super::super::heap::build_value(&mut caller, &mem, &json_arr)
                    }
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // ── std.cli — terminal I/O ───────────────────────────────────────

    // cli.read_line(prompt_ptr, prompt_len) -> i64 (String).
    // Pass len=0 to skip printing a prompt.
    linker
        .func_wrap(
            "env",
            "cli_read_line",
            |mut caller: Caller<'_, ()>, prompt_ptr: i32, prompt_len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                if prompt_len > 0 {
                    let prompt = {
                        let data = mem.data(&caller);
                        read_slice(data, prompt_ptr, prompt_len)
                    };
                    // No newline — the prompt is inline with the input.
                    use std::io::Write;
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(prompt.as_bytes());
                    let _ = out.flush();
                }
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).ok();
                let trimmed = input
                    .trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .to_string();
                wasm_alloc_str(&mut caller, &mem, &trimmed)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // cli.write(ptr, len) — no newline. Routes through the output sink
    // so captures see the bytes without a trailing \n.
    linker
        .func_wrap(
            "env",
            "cli_write",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let s = read_slice(data, ptr, len);
                // `stdout_line` adds a newline; write does not. Raw bytes
                // to the host stdout preserves VM parity.
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(s.as_bytes());
                let _ = out.flush();
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // cli.write_line(ptr, len) — same as print, but routes explicitly
    // through the shared output sink so tests can capture it.
    linker
        .func_wrap(
            "env",
            "cli_write_line",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = mem.data(&caller);
                let s = read_slice(data, ptr, len);
                output::stdout_line(&s);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // cli.clear() — ANSI clear screen + cursor home.
    linker
        .func_wrap("env", "cli_clear", |_caller: Caller<'_, ()>| {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(b"\x1b[2J\x1b[H");
            let _ = out.flush();
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // __fai_set_trap_msg(ptr, len) — Phase E: the guest writes an
    // assertion-failure message here then immediately traps with
    // `unreachable`. The CLI test runner reads the last stored message
    // via `take_trap_msg()` after catching the trap.
    linker
        .func_wrap(
            "env",
            "__fai_set_trap_msg",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
                let msg = if len <= 0 {
                    String::from("assertion failed")
                } else {
                    let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                    let data = mem.data(&caller);
                    read_slice(data, ptr, len)
                };
                set_trap_msg(msg);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // __fai_trap_report(code, a, b) — plan 116: structured trap reason.
    // The guest calls this with a `TRAP_*` code plus two payload words
    // right before executing `unreachable`; the host renders a readable
    // reason (decoding NaN-boxed values / heap headers from guest
    // memory) and stashes it like an assertion message, so the trap
    // surfaces as e.g. `over-release (rc -1) of String "id" at 0x3fa38`.
    linker
        .func_wrap(
            "env",
            "__fai_trap_report",
            |mut caller: Caller<'_, ()>, code: i32, a: i64, b: i64| {
                let mut msg = match caller.get_export("memory").and_then(|m| m.into_memory()) {
                    Some(mem) => format_trap_report(code, a, b, mem.data(&caller)),
                    None => format_trap_report(code, a, b, &[]),
                };
                // Attach the guest call chain (skipping runtime helpers)
                // so a named trap also says WHERE — the compact test
                // reporter shows only this message, not wasmtime's raw
                // backtrace, and a reason without a location is half a
                // diagnosis.
                let sites: Vec<String> = wasmtime::WasmBacktrace::capture(&caller)
                    .frames()
                    .iter()
                    .filter_map(|f| f.func_name())
                    .filter(|n| !n.starts_with("rt_") && !n.starts_with("__"))
                    .take(5)
                    .map(|s| s.to_string())
                    .collect();
                if !sites.is_empty() {
                    msg.push_str(&format!("\n    at {}", sites.join(" ← ")));
                }
                set_trap_msg(msg);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // __fai_rc_watch(obj_addr, rc_slot_addr, delta) — RC watchpoint.
    // rt_retain/rt_release call this on every RC op when FAI_RC_WATCH
    // codegen is on. The host filters to the single watched address
    // (FAI_RC_WATCH=<hex obj addr or rc-slot addr>) and logs the op, the
    // resulting rc, and a guest backtrace — the "who touches this
    // refcount" view that walks an over-release back to its unmatched op.
    linker
        .func_wrap(
            "env",
            "__fai_rc_watch",
            |mut caller: Caller<'_, ()>, obj_addr: i32, rc_slot: i32, delta: i32| {
                let want = match std::env::var("FAI_RC_WATCH").ok().and_then(|s| {
                    let t = s.trim().trim_start_matches("0x");
                    u32::from_str_radix(t, 16).ok()
                }) {
                    Some(w) => w,
                    None => return,
                };
                // Match either the logical object address or its rc-slot.
                if obj_addr as u32 != want && rc_slot as u32 != want {
                    return;
                }
                let mem = caller.get_export("memory").and_then(|e| e.into_memory());
                let rc_after = mem
                    .map(|m| {
                        let d = m.data(&caller);
                        let a = rc_slot as usize;
                        if a + 4 <= d.len() {
                            i32::from_le_bytes([d[a], d[a + 1], d[a + 2], d[a + 3]])
                        } else {
                            -999
                        }
                    })
                    .unwrap_or(-999);
                let sites: Vec<String> = wasmtime::WasmBacktrace::capture(&caller)
                    .frames()
                    .iter()
                    .filter_map(|fr| fr.func_name())
                    .filter(|n| !n.starts_with("rt_") && !n.starts_with("__"))
                    .take(6)
                    .map(|s| s.to_string())
                    .collect();
                let op = if delta > 0 { "retain" } else { "release" };
                eprintln!(
                    "[rc-watch 0x{:x}] {} -> rc={} at {}",
                    obj_addr,
                    op,
                    rc_after,
                    sites.join(" ← ")
                );
                use std::io::Write as _;
                let _ = std::io::stderr().flush();
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // __fai_mem_watch() — memory watchpoint. Called at every
    // alloc/retain/release when FAI_MEM_WATCH codegen is on. The host
    // reads the word at FAI_MEM_WATCH=<hex addr> and logs a backtrace
    // whenever it changes — the general "what touched this address" view
    // for a stray write to any field (a clobbered count, a header word),
    // not just an object's refcount. Poll granularity: the change is
    // reported at the first RC op / allocation after the write, so the
    // backtrace lands within a statement or two of the writer.
    linker
        .func_wrap("env", "__fai_mem_watch", |mut caller: Caller<'_, ()>| {
            let want = match std::env::var("FAI_MEM_WATCH").ok().and_then(|s| {
                let t = s.trim().trim_start_matches("0x");
                u32::from_str_radix(t, 16).ok()
            }) {
                Some(w) => w,
                None => return,
            };
            let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                Some(m) => m,
                None => return,
            };
            let cur = {
                let d = mem.data(&caller);
                let a = want as usize;
                if a + 4 > d.len() {
                    return;
                }
                i32::from_le_bytes([d[a], d[a + 1], d[a + 2], d[a + 3]])
            };
            // wasm memory is zero-initialised, so treat an unseen address
            // as 0 — then the first non-zero write is visible too.
            let prev = MEM_WATCH_LAST.with(|c| c.replace(Some(cur))).unwrap_or(0);
            if prev == cur {
                return;
            }
            let sites: Vec<String> = wasmtime::WasmBacktrace::capture(&caller)
                .frames()
                .iter()
                .filter_map(|fr| fr.func_name())
                .filter(|n| !n.starts_with("rt_") && !n.starts_with("__"))
                .take(6)
                .map(|s| s.to_string())
                .collect();
            let region = super::super::leak_ledger::describe_block(want)
                .map(|d| format!(" [{}]", d))
                .unwrap_or_default();
            eprintln!(
                "[mem-watch 0x{:x}] {} -> {} (0x{:x}){} at {}",
                want,
                prev,
                cur,
                cur as u32,
                region,
                sites.join(" ← ")
            );
            use std::io::Write as _;
            let _ = std::io::stderr().flush();
        })
        .map_err(|e| format!("linker error: {}", e))?;

    // __fai_alloc_event / __fai_free_event (plan 116 phase 5) — heap
    // allocation ledger feed. Only `--check-leaks` builds import these
    // (rt_alloc return paths / rt_free entry). The backtrace capture is
    // what buys allocation-site attribution (Tier 2a); it's expensive
    // per alloc, which is fine for a leak-hunt mode.
    linker
        .func_wrap(
            "env",
            "__fai_alloc_event",
            |mut caller: Caller<'_, ()>, addr: i32, size: i32| {
                use super::super::leak_ledger;
                if !leak_ledger::is_enabled() {
                    return;
                }
                let frames = leak_ledger::capture_frames(&caller);
                if leak_ledger::record_alloc(addr as u32, size as u32, false, frames) {
                    // Interval due — render with guest memory for tags.
                    let line = match caller.get_export("memory").and_then(|m| m.into_memory()) {
                        Some(mem) => leak_ledger::interval_report(mem.data(&caller)),
                        None => leak_ledger::interval_report(&[]),
                    };
                    if let Some(line) = line {
                        // Bypass the per-test output capture (CaptureGuard
                        // buffers host stdout/stderr so tests can assert on
                        // it) and write straight to the process stderr — a
                        // leak-hunt diagnostic must reach the terminal even
                        // mid-test, especially when chasing a runaway that
                        // never lets the test finish.
                        eprintln!("{}", line);
                        use std::io::Write as _;
                        let _ = std::io::stderr().flush();
                    }
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;
    linker
        .func_wrap(
            "env",
            "__fai_free_event",
            |_caller: Caller<'_, ()>, addr: i32, size: i32| {
                use super::super::leak_ledger;
                if leak_ledger::is_enabled() {
                    leak_ledger::record_free(addr as u32, size as u32);
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // __fai_ownership_event(op, site, value, aux) — phase-4 helper event
    // stream. Opt-in ownership-check runs record the tuple in the native
    // ownership ledger; default builds do not import this function.
    linker
        .func_wrap(
            "env",
            "__fai_ownership_event",
            |_caller: Caller<'_, ()>, op: i32, site: i32, value: i64, aux: i32| {
                super::super::ownership_balance::record_event(op, site, value, aux);
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // cli.move_to(row, col) — ANSI cursor position.
    linker
        .func_wrap(
            "env",
            "cli_move_to",
            |_caller: Caller<'_, ()>, row: i32, col: i32| {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                let _ = write!(out, "\x1b[{};{}H", row, col);
                let _ = out.flush();
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

fn read_slice(data: &[u8], ptr: i32, len: i32) -> String {
    let start = ptr as usize;
    let end = start.saturating_add(len as usize);
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

thread_local! {
    /// Last message written by the guest before trapping via the Phase E
    /// assertion-failure path. The test runner reads (and clears) this
    /// after catching a trap.
    static TRAP_MSG: std::cell::RefCell<Option<String>> = const {
        std::cell::RefCell::new(None)
    };
    /// Last value seen at the FAI_MEM_WATCH address, for change detection.
    static MEM_WATCH_LAST: std::cell::Cell<Option<i32>> = const {
        std::cell::Cell::new(None)
    };
}

pub(crate) fn set_trap_msg(msg: String) {
    TRAP_MSG.with(|c| *c.borrow_mut() = Some(msg));
}

pub(crate) fn take_trap_msg() -> Option<String> {
    TRAP_MSG.with(|c| c.borrow_mut().take())
}

// ── Trap-report rendering (plan 116) ─────────────────────────────────
// Codes and payload meanings are defined in
// `fai-codegen-wasm/src/runtime.rs` (`TRAP_*`); keep in sync with the
// JS twins in `fai-cli/src/lib.rs`.

/// Append the leak-ledger identity of the block at base address `a`
/// (logical pointer = base + 8) to a free-list trap message, when the
/// ledger is armed (`FAI_CHECK_LEAKS`). Naming the victim block usually
/// identifies the writer.
fn append_block_identity(msg: &mut String, base: i64) {
    let logical = (base as u32).wrapping_add(8);
    if let Some(desc) = super::super::leak_ledger::describe_block(logical) {
        msg.push_str(&format!(" — victim: {}", desc));
    }
}

/// Render a `__fai_trap_report(code, a, b)` into a readable reason.
fn format_trap_report(code: i32, a: i64, b: i64, data: &[u8]) -> String {
    use fai_codegen_wasm as cg;
    match code {
        c if c == cg::TRAP_RC_RETAIN_POISON => format!(
            "rc-check: retain of freed object at 0x{:x}",
            (a as u64) & ADDR_MASK,
        ),
        c if c == cg::TRAP_RC_RELEASE_POISON => format!(
            "rc-check: release of freed object at 0x{:x}",
            (a as u64) & ADDR_MASK,
        ),
        c if c == cg::TRAP_RC_OVER_RELEASE => {
            let mut msg = format!(
                "rc-check: over-release (rc {}) of {} at 0x{:x}",
                b,
                describe_boxed_value(data, a as u64),
                (a as u64) & ADDR_MASK,
            );
            // Name the object's allocation site from the leak ledger
            // (FAI_CHECK_LEAKS): the boxed value's logical pointer is its
            // object address, recorded as a live alloc. This says WHAT was
            // over-released and where it was born — the missing-retain site.
            let logical = (a as u64 & ADDR_MASK) as u32;
            if let Some(desc) = super::super::leak_ledger::describe_block(logical) {
                msg.push_str(&format!(" — {}", desc));
            }
            msg
        }
        c if c == cg::TRAP_OOM => format!(
            "out of memory: failed to grow linear memory ({} bytes requested, heap needs 0x{:x})",
            a, b,
        ),
        c if c == cg::TRAP_TASK_OVERFLOW => {
            format!("async task table full ({} of {} slots used)", a, b)
        }
        c if c == cg::TRAP_FORCE_UNWRAP_NULL => "force-unwrap (`!`) of null".to_string(),
        c if c == cg::TRAP_UNCAUGHT_ERROR => {
            format!("uncaught error: {}", describe_boxed_value(data, a as u64))
        }
        c if c == cg::TRAP_SCHED_STALL => format!(
            "scheduler stall: poll resumed {} tasks without quiescing (livelock; \
             task t{} was about to run again)",
            a, b,
        ),
        c if c == cg::TRAP_FREELIST_CORRUPT => {
            let mut msg = format!(
                "rc-check: corrupt free-list node 0x{:x} (heap_ptr 0x{:x}) — a freed \
                 block's link word was overwritten, or a garbage pointer was freed",
                a, b,
            );
            append_block_identity(&mut msg, a);
            msg
        }
        c if c == cg::TRAP_FREED_DIRTY => {
            // `b` packs (bucket_idx << 32 | tag_word); bucket idx 0 means
            // the report came from a path that only passes the tag.
            let tag = (b as u64) & 0xFFFF_FFFF;
            let bucket = (b as u64) >> 32;
            let mut msg = format!(
                "rc-check: freed block at 0x{:x} was written through a stale pointer \
                 while on the free list (tag word now 0x{:x}, expected poison)",
                a, tag,
            );
            if bucket > 0 {
                msg.push_str(&format!(" [block size {}B]", bucket * 8));
            }
            append_block_identity(&mut msg, a);
            msg
        }
        c if c == cg::TRAP_DOUBLE_FREE => {
            let mut msg = format!(
                "rc-check: double free of block at 0x{:x} (block size {})",
                a, b
            );
            append_block_identity(&mut msg, a);
            msg
        }
        c if c == cg::TRAP_INDEX_OOB => format!(
            "checked: index store out of bounds — xs[{}] = ... on an array of {} elements",
            a, b,
        ),
        c if c == cg::TRAP_DICT_CAP_INSANE => format!(
            "dict grow: implausible capacity {} (size word 0x{:x}) — dictionary.set \
             was handed a non-dict, stale, or mis-typed pointer",
            a, b,
        ),
        c if c == cg::TRAP_ALLOC_TOO_BIG => format!(
            "alloc-guard: single allocation of {} bytes ({} block) exceeds 256 MB — \
             runaway allocation (e.g. a string/array growing in a loop)",
            a, b,
        ),
        _ => format!("trap report (code {}, a=0x{:x}, b=0x{:x})", code, a, b),
    }
}

// NaN-box constants, mirrored from `wasm_runner::nan_box` (that module
// is `pub(super)` to the runner root and not visible here).
const QNAN: u64 = 0x7FFC_0000_0000_0000;
const SIGN_BIT: u64 = 0x8000_0000_0000_0000;
const TAG_MASK: u64 = 0x0007_0000_0000_0000;
const TAG_NULL: u64 = 0x0001_0000_0000_0000;
const TAG_VOID: u64 = 0x0002_0000_0000_0000;
const TAG_BOOL: u64 = 0x0003_0000_0000_0000;
const TAG_INT: u64 = 0x0004_0000_0000_0000;
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const OBJ_TAG_POISON: i32 = 0x7E_DEAD;

fn read_i32(data: &[u8], addr: usize) -> Option<i32> {
    let bytes = data.get(addr..addr + 4)?;
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

fn read_i64(data: &[u8], addr: usize) -> Option<i64> {
    let bytes = data.get(addr..addr + 8)?;
    Some(i64::from_le_bytes(bytes.try_into().ok()?))
}

/// The text of a NaN-boxed String object, or `None` if `v` isn't one.
fn string_text(data: &[u8], v: u64) -> Option<String> {
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return None;
    }
    let addr = (v & ADDR_MASK) as usize;
    if read_i32(data, addr)? != 0 {
        return None;
    }
    let len = (read_i32(data, addr + 4)?.max(0) as usize).min(40);
    data.get(addr + 8..addr + 8 + len)
        .map(|b| String::from_utf8_lossy(b).into_owned())
}

/// Short human description of a NaN-boxed value, reading the object
/// header (tag, count, rc) from guest memory when it's a heap object.
/// Strings include a truncated preview — naming the corrupted object is
/// what turns an RC trap from a memory dump into a bug report.
fn describe_boxed_value(data: &[u8], v: u64) -> String {
    if v == QNAN | TAG_NULL {
        return "null".to_string();
    }
    if v == QNAN | TAG_VOID {
        return "void".to_string();
    }
    if (v & (QNAN | SIGN_BIT | TAG_MASK)) == (QNAN | TAG_BOOL) {
        return format!("Bool {}", (v & 1) == 1);
    }
    if (v & (QNAN | SIGN_BIT | TAG_MASK)) == (QNAN | TAG_INT) {
        return format!("Int {}", v as u32 as i32);
    }
    if (v & QNAN) != QNAN {
        return format!("Float {}", f64::from_bits(v));
    }
    if (v & (QNAN | SIGN_BIT)) != (QNAN | SIGN_BIT) {
        return format!("<unknown value 0x{:x}>", v);
    }
    let addr = (v & ADDR_MASK) as usize;
    let Some(tag) = read_i32(data, addr) else {
        return format!("<object at 0x{:x}, out of bounds>", addr);
    };
    let count = read_i32(data, addr + 4).unwrap_or(-1);
    let rc = if addr >= 8 {
        read_i32(data, addr - 8)
    } else {
        None
    };
    let rc_suffix = rc.map(|r| format!(", rc {}", r)).unwrap_or_default();
    match tag {
        0 => {
            let len = count.max(0) as usize;
            let preview_len = len.min(40);
            let preview = data
                .get(addr + 8..addr + 8 + preview_len)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let ellipsis = if len > preview_len { "…" } else { "" };
            format!(
                "String \"{}{}\" ({} bytes{})",
                preview, ellipsis, len, rc_suffix
            )
        }
        1 => format!("Array({} items{})", count, rc_suffix),
        2 => format!("Tuple({} items{})", count, rc_suffix),
        3 => {
            // Shallow entry preview — an uncaught `throw Error('boom')`
            // is a Dict, and `{message: "boom"}` is the bug report.
            let shown = (count.max(0) as usize).min(3);
            let mut parts = Vec::with_capacity(shown);
            for i in 0..shown {
                let entry = addr + 8 + i * 16;
                let key = read_i64(data, entry).map(|k| {
                    // Keys are Strings in practice — render the bare text.
                    string_text(data, k as u64)
                        .unwrap_or_else(|| describe_boxed_value(data, k as u64))
                });
                let val = read_i64(data, entry + 8).map(|v| describe_boxed_value(data, v as u64));
                if let (Some(k), Some(v)) = (key, val) {
                    parts.push(format!("{}: {}", k, v));
                }
            }
            let ellipsis = if count.max(0) as usize > shown {
                ", …"
            } else {
                ""
            };
            format!(
                "Dict({} entries{}) {{{}{}}}",
                count,
                rc_suffix,
                parts.join(", "),
                ellipsis,
            )
        }
        4 => format!("Closure(table slot {}{})", count, rc_suffix),
        6 => format!("NativeFn(method {}{})", count, rc_suffix),
        t if t == OBJ_TAG_POISON => format!("freed object (poisoned{})", rc_suffix),
        t => format!("<object tag {} at 0x{:x}{}>", t, addr, rc_suffix),
    }
}

#[cfg(test)]
mod trap_report_tests {
    use super::*;

    fn boxed_obj(addr: u32) -> u64 {
        QNAN | SIGN_BIT | addr as u64
    }

    #[test]
    fn describes_string_with_preview_and_rc() {
        // memory: rc prefix at 8, header at 16: tag=0, len=2, bytes "id"
        let mut data = vec![0u8; 64];
        data[8..12].copy_from_slice(&1i32.to_le_bytes()); // rc = 1
        data[16..20].copy_from_slice(&0i32.to_le_bytes()); // tag = string
        data[20..24].copy_from_slice(&2i32.to_le_bytes()); // len = 2
        data[24] = b'i';
        data[25] = b'd';
        let desc = describe_boxed_value(&data, boxed_obj(16));
        assert_eq!(desc, "String \"id\" (2 bytes, rc 1)");
    }

    #[test]
    fn over_release_report_names_the_object() {
        let mut data = vec![0u8; 64];
        data[8..12].copy_from_slice(&(-1i32).to_le_bytes());
        data[16..20].copy_from_slice(&0i32.to_le_bytes());
        data[20..24].copy_from_slice(&2i32.to_le_bytes());
        data[24] = b'i';
        data[25] = b'd';
        let msg = format_trap_report(
            fai_codegen_wasm::TRAP_RC_OVER_RELEASE,
            boxed_obj(16) as i64,
            -1,
            &data,
        );
        assert!(msg.contains("over-release (rc -1)"), "{msg}");
        assert!(msg.contains("String \"id\""), "{msg}");
        assert!(msg.contains("0x10"), "{msg}");
    }

    #[test]
    fn force_unwrap_and_oom_reports() {
        assert_eq!(
            format_trap_report(fai_codegen_wasm::TRAP_FORCE_UNWRAP_NULL, 0, 0, &[]),
            "force-unwrap (`!`) of null"
        );
        let oom = format_trap_report(fai_codegen_wasm::TRAP_OOM, 1024, 0x200000, &[]);
        assert!(oom.contains("1024 bytes requested"), "{oom}");
    }
}
