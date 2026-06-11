//! Built-in function type signatures.
//!
//! Each module installs a category of builtins into a shared HashMap.
//! The split is purely organizational — the public API is unchanged.

use std::collections::HashMap;

use crate::types::*;

mod array;
mod browser;
mod concurrency;
mod core;
mod crypto;
mod dict;
mod env;
mod events;
mod ffi;
mod file;
mod http;
mod json;
mod math;
mod net;
mod process;
mod storage;
mod string;
mod test;
mod time_log;

/// Install all builtin function signatures into a map.
pub fn install_builtins() -> HashMap<String, Type> {
    let mut b = HashMap::new();
    core::install(&mut b);
    array::install(&mut b);
    string::install(&mut b);
    dict::install(&mut b);
    json::install(&mut b);
    file::install(&mut b);
    env::install(&mut b);
    events::install(&mut b);
    time_log::install(&mut b);
    math::install(&mut b);
    browser::install(&mut b);
    http::install(&mut b);
    net::install(&mut b);
    crypto::install(&mut b);
    process::install(&mut b);
    storage::install(&mut b);
    test::install(&mut b);
    ffi::install(&mut b);
    concurrency::install(&mut b);
    b
}

// ── Shared helpers ─────────────────────────────────────────────────────

pub(super) fn p(name: &str, ty: Type) -> FunctionParam {
    param(name, ty)
}

pub(super) fn pd(name: &str, ty: Type) -> FunctionParam {
    param_default(name, ty)
}

pub(super) fn ins(
    map: &mut HashMap<String, Type>,
    name: &str,
    params: &[FunctionParam],
    returns: &[Type],
) {
    map.insert(
        name.to_string(),
        function_type(name, params.to_vec(), returns.to_vec()),
    );
}

pub(super) fn ins_d(
    map: &mut HashMap<String, Type>,
    name: &str,
    params: &[FunctionParam],
    returns: &[Type],
) {
    ins(map, name, params, returns);
}

/// Static documentation entry for a stdlib builtin.
pub struct BuiltinDoc {
    /// Dotted module path, e.g. `"std.array"`.
    pub module: &'static str,
    /// Exported name within the module, e.g. `"join"`.
    pub name: &'static str,
    /// Internal builtin key used for type lookup, e.g. `"arrayJoin"`.
    pub builtin_name: &'static str,
    /// One-line description.
    pub doc: &'static str,
}

/// All stdlib builtin documentation entries, one per exported function.
pub fn all_builtin_docs() -> &'static [BuiltinDoc] {
    &[
        // std.array
        BuiltinDoc { module: "std.array", name: "append",   builtin_name: "append",       doc: "Return a new array with the item appended." },
        BuiltinDoc { module: "std.array", name: "length",   builtin_name: "length",        doc: "Return the number of items in an array." },
        BuiltinDoc { module: "std.array", name: "isEmpty",  builtin_name: "isEmpty",       doc: "Return true if the array is empty." },
        BuiltinDoc { module: "std.array", name: "first",    builtin_name: "first",         doc: "Return the first item, or null if empty." },
        BuiltinDoc { module: "std.array", name: "last",     builtin_name: "last",          doc: "Return the last item, or null if empty." },
        BuiltinDoc { module: "std.array", name: "map",      builtin_name: "map",           doc: "Apply a function to each item and return a new array." },
        BuiltinDoc { module: "std.array", name: "filter",   builtin_name: "filter",        doc: "Return items for which the predicate returns true." },
        BuiltinDoc { module: "std.array", name: "find",     builtin_name: "find",          doc: "Return the first item matching the predicate, or null." },
        BuiltinDoc { module: "std.array", name: "isAny",    builtin_name: "isAny",         doc: "Return true if any item satisfies the predicate." },
        BuiltinDoc { module: "std.array", name: "isAll",    builtin_name: "isAll",         doc: "Return true if all items satisfy the predicate." },
        BuiltinDoc { module: "std.array", name: "contains", builtin_name: "arrayContains", doc: "Return true if the array contains the given value." },
        BuiltinDoc { module: "std.array", name: "sort",     builtin_name: "arraySort",     doc: "Return a sorted copy of the array." },
        BuiltinDoc { module: "std.array", name: "reverse",  builtin_name: "arrayReverse",  doc: "Return the array in reverse order." },
        BuiltinDoc { module: "std.array", name: "indexOf",  builtin_name: "arrayIndexOf",  doc: "Return the index of a value, or -1 if not found." },
        BuiltinDoc { module: "std.array", name: "join",     builtin_name: "arrayJoin",     doc: "Join string items into a single string." },
        BuiltinDoc { module: "std.array", name: "slice",    builtin_name: "arraySlice",    doc: "Return a sub-array from start to end (exclusive)." },

        // std.math
        BuiltinDoc { module: "std.math", name: "random", builtin_name: "mathRandom", doc: "Return a random Float between 0.0 and 1.0." },
        BuiltinDoc { module: "std.math", name: "floor",  builtin_name: "mathFloor",  doc: "Round a Float down to the nearest integer." },
        BuiltinDoc { module: "std.math", name: "ceil",   builtin_name: "mathCeil",   doc: "Round a Float up to the nearest integer." },
        BuiltinDoc { module: "std.math", name: "round",  builtin_name: "mathRound",  doc: "Round a Float to the nearest integer." },
        BuiltinDoc { module: "std.math", name: "abs",    builtin_name: "mathAbs",    doc: "Return the absolute value of a Float." },
        BuiltinDoc { module: "std.math", name: "min",    builtin_name: "mathMin",    doc: "Return the smaller of two Floats." },
        BuiltinDoc { module: "std.math", name: "max",    builtin_name: "mathMax",    doc: "Return the larger of two Floats." },
        BuiltinDoc { module: "std.math", name: "sqrt",   builtin_name: "mathSqrt",   doc: "Return the square root of a Float." },
        BuiltinDoc { module: "std.math", name: "pow",    builtin_name: "mathPow",    doc: "Raise base to the power of exp." },

        // std.string
        BuiltinDoc { module: "std.string", name: "length",     builtin_name: "length",          doc: "Return the number of characters in a string." },
        BuiltinDoc { module: "std.string", name: "isEmpty",    builtin_name: "isEmpty",          doc: "Return true if the string is empty." },
        BuiltinDoc { module: "std.string", name: "replace",    builtin_name: "replace",          doc: "Replace all occurrences of find with with." },
        BuiltinDoc { module: "std.string", name: "split",      builtin_name: "split",            doc: "Split a string on a separator into an array." },
        BuiltinDoc { module: "std.string", name: "trim",       builtin_name: "trim",             doc: "Remove leading and trailing whitespace." },
        BuiltinDoc { module: "std.string", name: "toUpper",    builtin_name: "toUpper",          doc: "Convert the string to uppercase." },
        BuiltinDoc { module: "std.string", name: "toLower",    builtin_name: "toLower",          doc: "Convert the string to lowercase." },
        BuiltinDoc { module: "std.string", name: "contains",   builtin_name: "stringContains",   doc: "Return true if the string contains the given substring." },
        BuiltinDoc { module: "std.string", name: "startsWith", builtin_name: "stringStartsWith", doc: "Return true if the string starts with the prefix." },
        BuiltinDoc { module: "std.string", name: "endsWith",   builtin_name: "stringEndsWith",   doc: "Return true if the string ends with the suffix." },
        BuiltinDoc { module: "std.string", name: "substring",  builtin_name: "stringSubstring",  doc: "Return a substring from start to end." },
        BuiltinDoc { module: "std.string", name: "indexOf",    builtin_name: "stringIndexOf",    doc: "Return the index of the first occurrence, or -1." },
        BuiltinDoc { module: "std.string", name: "join",       builtin_name: "stringJoin",       doc: "Join an array of strings with a separator." },
        BuiltinDoc { module: "std.string", name: "repeat",     builtin_name: "stringRepeat",     doc: "Repeat a string count times." },
        BuiltinDoc { module: "std.string", name: "trimStart",  builtin_name: "stringTrimStart",  doc: "Remove leading whitespace." },
        BuiltinDoc { module: "std.string", name: "trimEnd",    builtin_name: "stringTrimEnd",    doc: "Remove trailing whitespace." },

        // std.convert
        BuiltinDoc { module: "std.convert", name: "toString",   builtin_name: "toString",   doc: "Convert any value to its string representation." },
        BuiltinDoc { module: "std.convert", name: "toInt",      builtin_name: "toInt",      doc: "Convert a value to an integer." },
        BuiltinDoc { module: "std.convert", name: "toFloat",    builtin_name: "toFloat",    doc: "Convert a value to a float." },
        BuiltinDoc { module: "std.convert", name: "toBool",     builtin_name: "toBool",     doc: "Convert a value to a boolean." },
        BuiltinDoc { module: "std.convert", name: "parseInt",   builtin_name: "parseInt",   doc: "Parse an integer from a string." },
        BuiltinDoc { module: "std.convert", name: "parseFloat", builtin_name: "parseFloat", doc: "Parse a float from a string." },

        // std.dictionary
        BuiltinDoc { module: "std.dictionary", name: "get",       builtin_name: "get",       doc: "Look up a value by key, returns null if missing." },
        BuiltinDoc { module: "std.dictionary", name: "getString",  builtin_name: "getString", doc: "Look up a String value by key." },
        BuiltinDoc { module: "std.dictionary", name: "getInt",     builtin_name: "getInt",    doc: "Look up an Int value by key." },
        BuiltinDoc { module: "std.dictionary", name: "getBool",    builtin_name: "getBool",   doc: "Look up a Bool value by key." },
        BuiltinDoc { module: "std.dictionary", name: "set",        builtin_name: "set",       doc: "Set a key-value pair and return the updated dictionary." },
        BuiltinDoc { module: "std.dictionary", name: "getKeys",    builtin_name: "getKeys",   doc: "Return all keys in the dictionary as a String array." },
        BuiltinDoc { module: "std.dictionary", name: "hasKey",     builtin_name: "hasKey",    doc: "Return true if the key exists in the dictionary." },

        // std.error
        BuiltinDoc { module: "std.error", name: "Error",   builtin_name: "Error",   doc: "Create a new Error with the given message." },
        BuiltinDoc { module: "std.error", name: "message", builtin_name: "message", doc: "Return the error message string." },
        BuiltinDoc { module: "std.error", name: "kind",    builtin_name: "kind",    doc: "Return the error kind string." },
        BuiltinDoc { module: "std.error", name: "isError", builtin_name: "isError", doc: "Return true if the value is an Error." },
        BuiltinDoc { module: "std.error", name: "unwrap",  builtin_name: "unwrap",  doc: "Return value if not null/error, otherwise return fallback." },

        // std.io
        BuiltinDoc { module: "std.io", name: "print", builtin_name: "print", doc: "Print a value to stdout." },

        // std.json
        BuiltinDoc { module: "std.json", name: "parse",         builtin_name: "jsonParse",         doc: "Parse a JSON string into a value." },
        BuiltinDoc { module: "std.json", name: "stringify",     builtin_name: "jsonStringify",     doc: "Serialize a value to a JSON string." },
        BuiltinDoc { module: "std.json", name: "requireString", builtin_name: "jsonRequireString", doc: "Extract a required string field from a JSON object." },

        // std.html
        BuiltinDoc { module: "std.html", name: "escape", builtin_name: "htmlEscape", doc: "Escape special HTML characters in a string." },

        // std.browser
        BuiltinDoc { module: "std.browser", name: "setHtml", builtin_name: "setHtml", doc: "Replace the browser app root contents with HTML." },
        BuiltinDoc { module: "std.browser", name: "setHtmlAt", builtin_name: "setHtmlAt", doc: "Replace a selected browser element with HTML." },
        BuiltinDoc { module: "std.browser", name: "getLocationPath", builtin_name: "getLocationPath", doc: "Return the browser location path." },
        BuiltinDoc { module: "std.browser", name: "pushHistoryState", builtin_name: "pushHistoryState", doc: "Push a browser history path." },
        BuiltinDoc { module: "std.browser", name: "remoteCall", builtin_name: "remoteCall", doc: "Call a forai remote endpoint from browser code." },

        // std.file
        BuiltinDoc { module: "std.file", name: "read",   builtin_name: "fileRead",   doc: "Read the entire contents of a file as a string." },
        BuiltinDoc { module: "std.file", name: "write",  builtin_name: "fileWrite",  doc: "Write text to a file. Returns true on success." },
        BuiltinDoc { module: "std.file", name: "exists", builtin_name: "fileExists", doc: "Return true if the file exists." },
        BuiltinDoc { module: "std.file", name: "list",   builtin_name: "fileList",   doc: "List all file names in a directory." },

        // std.process
        BuiltinDoc { module: "std.process", name: "available", builtin_name: "processAvailable", doc: "Return true if process execution is available on this host (native only)." },
        BuiltinDoc { module: "std.process", name: "run",   builtin_name: "processRun",   doc: "Run a bash command and return a JSON result string." },
        BuiltinDoc { module: "std.process", name: "start", builtin_name: "processStart", doc: "Start a bash command session and return a JSON result string." },
        BuiltinDoc { module: "std.process", name: "write", builtin_name: "processWrite", doc: "Write input to a process session and return a JSON result string." },
        BuiltinDoc { module: "std.process", name: "read",  builtin_name: "processRead",  doc: "Read buffered output from a process session and return a JSON result string." },
        BuiltinDoc { module: "std.process", name: "stop",  builtin_name: "processStop",  doc: "Stop a process session and return a JSON result string." },

        // std.path
        BuiltinDoc { module: "std.path", name: "join",     builtin_name: "pathJoin",     doc: "Join two path segments." },
        BuiltinDoc { module: "std.path", name: "dirname",  builtin_name: "pathDirname",  doc: "Return the directory portion of a path." },
        BuiltinDoc { module: "std.path", name: "basename", builtin_name: "pathBasename", doc: "Return the filename portion of a path." },
        BuiltinDoc { module: "std.path", name: "extname",  builtin_name: "pathExtname",  doc: "Return the file extension, including the leading dot." },

        // std.env
        BuiltinDoc { module: "std.env", name: "get",  builtin_name: "envGet",  doc: "Read a process environment variable, or null if unset." },
        BuiltinDoc { module: "std.env", name: "load", builtin_name: "envLoad", doc: "Parse a dotenv-style file and merge its entries into the process environment. Returns true on success, false if the file is missing or unreadable." },

        // std.events
        BuiltinDoc { module: "std.events", name: "on",          builtin_name: "eventOn",          doc: "Register a handler for an event name. Returns a Subscription handle." },
        BuiltinDoc { module: "std.events", name: "once",        builtin_name: "eventOnce",        doc: "Register a handler that fires once and is then auto-removed." },
        BuiltinDoc { module: "std.events", name: "off",         builtin_name: "eventOff",         doc: "Cancel a Subscription. Returns true if it was active, false if already removed." },
        BuiltinDoc { module: "std.events", name: "emit",        builtin_name: "eventEmit",        doc: "Synchronously deliver an event to every subscriber registered under name." },
        BuiltinDoc { module: "std.events", name: "subscribers", builtin_name: "eventSubscribers", doc: "Return the number of active subscribers for an event name." },
        BuiltinDoc { module: "std.events", name: "clear",       builtin_name: "eventClear",       doc: "Remove every subscriber registered under name." },
        BuiltinDoc { module: "std.events", name: "clearAll",    builtin_name: "eventClearAll",    doc: "Remove every subscription across every event name. Test cleanup helper." },

        // std.time
        BuiltinDoc { module: "std.time", name: "now",  builtin_name: "timeNow",  doc: "Return the current time as milliseconds since the Unix epoch." },
        BuiltinDoc { module: "std.time", name: "unix", builtin_name: "timeUnix", doc: "Return the current Unix timestamp in seconds." },

        // std.log
        BuiltinDoc { module: "std.log", name: "info",  builtin_name: "logInfo",  doc: "Log an informational message." },
        BuiltinDoc { module: "std.log", name: "warn",  builtin_name: "logWarn",  doc: "Log a warning message." },
        BuiltinDoc { module: "std.log", name: "error", builtin_name: "logError", doc: "Log an error message." },

        // std.cli
        BuiltinDoc { module: "std.cli", name: "readLine",  builtin_name: "cliReadLine",  doc: "Read a line of input from stdin." },
        BuiltinDoc { module: "std.cli", name: "write",     builtin_name: "cliWrite",     doc: "Write a value to stdout without a newline." },
        BuiltinDoc { module: "std.cli", name: "writeLine", builtin_name: "cliWriteLine", doc: "Write a value to stdout followed by a newline." },
        BuiltinDoc { module: "std.cli", name: "clear",     builtin_name: "cliClear",     doc: "Clear the terminal screen." },
        BuiltinDoc { module: "std.cli", name: "moveTo",    builtin_name: "cliMoveTo",    doc: "Move the terminal cursor to the given row and column." },

        // std.test
        BuiltinDoc { module: "std.test", name: "assert", builtin_name: "testAssert", doc: "Assert a condition is true, with an optional message." },
        BuiltinDoc { module: "std.test", name: "equal",  builtin_name: "testEqual",  doc: "Assert two values are equal, with an optional message." },

        // std.http.request
        BuiltinDoc { module: "std.http.request", name: "get",    builtin_name: "httpRequestGet",    doc: "Send an HTTP GET request." },
        BuiltinDoc { module: "std.http.request", name: "post",   builtin_name: "httpRequestPost",   doc: "Send an HTTP POST request with a body." },
        BuiltinDoc { module: "std.http.request", name: "put",    builtin_name: "httpRequestPut",    doc: "Send an HTTP PUT request with a body." },
        BuiltinDoc { module: "std.http.request", name: "patch",  builtin_name: "httpRequestPatch",  doc: "Send an HTTP PATCH request with a body." },
        BuiltinDoc { module: "std.http.request", name: "delete", builtin_name: "httpRequestDelete", doc: "Send an HTTP DELETE request." },

        // std.http.server
        BuiltinDoc { module: "std.http.server", name: "ok",         builtin_name: "httpServerOk",             doc: "Return a 200 OK HTTP response with the given body." },
        BuiltinDoc { module: "std.http.server", name: "text",       builtin_name: "httpServerText",           doc: "Return a plain text HTTP response." },
        BuiltinDoc { module: "std.http.server", name: "html",       builtin_name: "httpServerHtml",           doc: "Return an HTML HTTP response." },
        BuiltinDoc { module: "std.http.server", name: "json",       builtin_name: "httpServerJson",           doc: "Return a JSON HTTP response." },
        BuiltinDoc { module: "std.http.server", name: "redirect",   builtin_name: "httpServerRedirect",       doc: "Return an HTTP redirect response." },
        BuiltinDoc { module: "std.http.server", name: "router",     builtin_name: "httpServerRouter",         doc: "Create a new HTTP router." },
        BuiltinDoc { module: "std.http.server", name: "get",        builtin_name: "httpServerRouterGet",      doc: "Register a GET route handler on a router." },
        BuiltinDoc { module: "std.http.server", name: "post",       builtin_name: "httpServerRouterPost",     doc: "Register a POST route handler on a router." },
        BuiltinDoc { module: "std.http.server", name: "serveFiles", builtin_name: "httpServerRouterServeFiles", doc: "Serve static files from a directory." },
        BuiltinDoc { module: "std.http.server", name: "listen",     builtin_name: "httpServerRouterListen",   doc: "Start the HTTP server on the given port." },

        // std.ffi
        BuiltinDoc { module: "std.ffi", name: "available", builtin_name: "ffiAvailable", doc: "Return true if the given native library is available." },

        // std.net
        BuiltinDoc { module: "std.net", name: "available", builtin_name: "netAvailable", doc: "Return true if a network connection is available." },

        // std.net.tcp
        BuiltinDoc { module: "std.net.tcp", name: "listen",   builtin_name: "netTcpListen",   doc: "Open a TCP listener on the given port. Returns a handle." },
        BuiltinDoc { module: "std.net.tcp", name: "accept",   builtin_name: "netTcpAccept",   doc: "Accept an incoming TCP connection." },
        BuiltinDoc { module: "std.net.tcp", name: "connect",  builtin_name: "netTcpConnect",  doc: "Connect to a TCP server. Returns a handle." },
        BuiltinDoc { module: "std.net.tcp", name: "read",     builtin_name: "netTcpRead",     doc: "Read available data from a TCP connection." },
        BuiltinDoc { module: "std.net.tcp", name: "readLine", builtin_name: "netTcpReadLine", doc: "Read one line from a TCP connection." },
        BuiltinDoc { module: "std.net.tcp", name: "write",    builtin_name: "netTcpWrite",    doc: "Write data to a TCP connection. Returns bytes written." },
        BuiltinDoc { module: "std.net.tcp", name: "close",    builtin_name: "netTcpClose",    doc: "Close a TCP connection or listener." },
        BuiltinDoc { module: "std.net.tcp", name: "address",  builtin_name: "netTcpAddress",  doc: "Return the remote address of a TCP connection." },

        // std.net.udp
        BuiltinDoc { module: "std.net.udp", name: "bind",      builtin_name: "netUdpBind",      doc: "Bind a UDP socket to the given port. Returns a handle." },
        BuiltinDoc { module: "std.net.udp", name: "send",      builtin_name: "netUdpSend",      doc: "Send a UDP datagram to a host:port. Returns bytes sent." },
        BuiltinDoc { module: "std.net.udp", name: "receive",   builtin_name: "netUdpReceive",   doc: "Receive a UDP datagram." },
        BuiltinDoc { module: "std.net.udp", name: "close",     builtin_name: "netUdpClose",     doc: "Close a UDP socket." },
        BuiltinDoc { module: "std.net.udp", name: "broadcast", builtin_name: "netUdpBroadcast", doc: "Enable or disable UDP broadcast mode." },

        // std.crypto
        BuiltinDoc { module: "std.crypto", name: "available",          builtin_name: "cryptoAvailable",          doc: "Return true if crypto primitives are available on this host (native only)." },
        BuiltinDoc { module: "std.crypto", name: "hmacSha256Hex",      builtin_name: "cryptoHmacSha256Hex",      doc: "Return the lowercase hex HMAC-SHA256 of message under key (both UTF-8)." },
        BuiltinDoc { module: "std.crypto", name: "sha256Hex",          builtin_name: "cryptoSha256Hex",          doc: "Return the lowercase hex SHA-256 digest of the UTF-8 input." },
        BuiltinDoc { module: "std.crypto", name: "hexEncode",          builtin_name: "cryptoHexEncode",          doc: "Return the lowercase hex encoding of the UTF-8 input bytes." },
        BuiltinDoc { module: "std.crypto", name: "constantTimeEquals", builtin_name: "cryptoConstantTimeEquals", doc: "Compare two strings in constant time. False on length mismatch." },
        BuiltinDoc { module: "std.crypto", name: "base64Encode",       builtin_name: "cryptoBase64Encode",       doc: "Return the standard padded base64 encoding of the UTF-8 input." },
        BuiltinDoc { module: "std.crypto", name: "base64Decode",       builtin_name: "cryptoBase64Decode",       doc: "Decode standard base64 and return the bytes as a UTF-8 (lossy) string. Empty on invalid input." },

        // std.storage
        BuiltinDoc { module: "std.storage", name: "storageGet",    builtin_name: "storageGet",    doc: "Read a value for a key from the platform's persistent store. Returns null if absent." },
        BuiltinDoc { module: "std.storage", name: "storageSet",    builtin_name: "storageSet",    doc: "Write a value under a key into the platform's persistent store." },
        BuiltinDoc { module: "std.storage", name: "storageRemove", builtin_name: "storageRemove", doc: "Delete the entry for the given key." },
        BuiltinDoc { module: "std.storage", name: "storageClear",  builtin_name: "storageClear",  doc: "Remove every entry in the store." },
    ]
}

/// Build the `assert` namespace used in test blocks.
pub fn assert_namespace(builtins: &HashMap<String, Type>) -> Type {
    let mut exports = HashMap::new();
    if let Some(t) = builtins.get("testEqual") {
        exports.insert("equals".to_string(), t.clone());
        exports.insert("equal".to_string(), t.clone());
    }
    if let Some(t) = builtins.get("testAssert") {
        exports.insert("isTrue".to_string(), t.clone());
    }
    if let Some(t) = builtins.get("testAssertFalse") {
        exports.insert("isFalse".to_string(), t.clone());
    }
    if let Some(t) = builtins.get("testAssertNull") {
        exports.insert("isNull".to_string(), t.clone());
    }
    if let Some(t) = builtins.get("testAssertNotNull") {
        exports.insert("isNotNull".to_string(), t.clone());
    }
    if let Some(t) = builtins.get("assertCalledWith") {
        exports.insert("calledWith".to_string(), t.clone());
    }
    if let Some(t) = builtins.get("assertCallCount") {
        exports.insert("callCount".to_string(), t.clone());
    }
    if let Some(t) = builtins.get("assertNotCalled") {
        exports.insert("notCalled".to_string(), t.clone());
    }
    Type::ModuleNamespace {
        name: "assert".to_string(),
        exports,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_builtin<'a>(b: &'a HashMap<String, Type>, name: &str) -> &'a Type {
        b.get(name)
            .unwrap_or_else(|| panic!("missing builtin: {}", name))
    }

    fn fn_sig(t: &Type) -> &FunctionSig {
        match t {
            Type::Function(sig) => sig,
            _ => panic!("expected Function type, got: {:?}", t),
        }
    }

    #[test]
    fn test_install_builtins_returns_nonempty_map() {
        let b = install_builtins();
        assert!(b.len() > 50, "expected many builtins, got {}", b.len());
    }

    #[test]
    fn test_assert_namespace_exports() {
        let b = install_builtins();
        let ns = assert_namespace(&b);
        match ns {
            Type::ModuleNamespace { name, exports } => {
                assert_eq!(name, "assert");
                assert!(exports.contains_key("equals"));
                assert!(exports.contains_key("equal"));
                assert!(exports.contains_key("isTrue"));
                assert!(exports.contains_key("isFalse"));
                assert!(exports.contains_key("calledWith"));
                assert!(exports.contains_key("callCount"));
                assert!(exports.contains_key("notCalled"));
            }
            _ => panic!("expected ModuleNamespace"),
        }
    }

    #[test]
    fn test_assert_namespace_with_empty_builtins() {
        let empty = HashMap::new();
        let ns = assert_namespace(&empty);
        match ns {
            Type::ModuleNamespace { name, exports } => {
                assert_eq!(name, "assert");
                assert!(exports.is_empty());
            }
            _ => panic!("expected ModuleNamespace"),
        }
    }

    #[test]
    fn test_helper_p() {
        let param = p("name", Type::String);
        assert_eq!(param.name, "name");
        assert!(matches!(param.ty, Type::String));
        assert!(!param.has_default);
    }

    #[test]
    fn test_helper_pd() {
        let param = pd("name", Type::String);
        assert_eq!(param.name, "name");
        assert!(param.has_default);
    }

    #[test]
    fn test_helper_ins() {
        let mut b = HashMap::new();
        ins(&mut b, "test_fn", &[p("x", Type::Int)], &[Type::Bool]);
        let sig = fn_sig(check_builtin(&b, "test_fn"));
        assert_eq!(sig.name, "test_fn");
        assert_eq!(sig.params.len(), 1);
        assert!(matches!(sig.returns[0], Type::Bool));
    }

    #[test]
    fn test_helper_ins_d() {
        let mut b = HashMap::new();
        ins_d(&mut b, "test_fn", &[pd("opt", Type::String)], &[Type::Void]);
        let sig = fn_sig(check_builtin(&b, "test_fn"));
        assert!(sig.params[0].has_default);
    }
}
