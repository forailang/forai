//! I/O host imports: `print`, `read_file`, `write_file`, `set_html`,
//! `set_html_at`, `file_exists`, `log_*`, `path_*`, `html_escape`.

use wasmtime::*;

use super::super::heap::wasm_alloc_str;
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
}

pub(crate) fn set_trap_msg(msg: String) {
    TRAP_MSG.with(|c| *c.borrow_mut() = Some(msg));
}

pub(crate) fn take_trap_msg() -> Option<String> {
    TRAP_MSG.with(|c| c.borrow_mut().take())
}
