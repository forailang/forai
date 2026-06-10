//! Standard module export maps.
//!
//! Maps `std.xxx` module names to their exported names → builtin names.

use std::collections::HashMap;

/// Returns a map of std module name → (export name → builtin name).
pub fn std_module_exports() -> HashMap<String, Vec<(String, String)>> {
    let mut m = HashMap::new();

    m.insert(
        "std.array".into(),
        vec![
            ("append", "append"),
            ("length", "length"),
            ("isEmpty", "isEmpty"),
            ("first", "first"),
            ("last", "last"),
            ("map", "map"),
            ("filter", "filter"),
            ("find", "find"),
            ("isAny", "isAny"),
            ("isAll", "isAll"),
            ("contains", "arrayContains"),
            ("sort", "arraySort"),
            ("reverse", "arrayReverse"),
            ("indexOf", "arrayIndexOf"),
            ("join", "arrayJoin"),
            ("slice", "arraySlice"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.math".into(),
        vec![
            ("random", "mathRandom"),
            ("floor", "mathFloor"),
            ("ceil", "mathCeil"),
            ("round", "mathRound"),
            ("abs", "mathAbs"),
            ("min", "mathMin"),
            ("max", "mathMax"),
            ("sqrt", "mathSqrt"),
            ("pow", "mathPow"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.string".into(),
        vec![
            ("length", "length"),
            ("isEmpty", "isEmpty"),
            ("replace", "replace"),
            ("split", "split"),
            ("trim", "trim"),
            ("toUpper", "toUpper"),
            ("toLower", "toLower"),
            ("contains", "stringContains"),
            ("startsWith", "stringStartsWith"),
            ("endsWith", "stringEndsWith"),
            ("substring", "stringSubstring"),
            ("indexOf", "stringIndexOf"),
            ("join", "stringJoin"),
            ("repeat", "stringRepeat"),
            ("trimStart", "stringTrimStart"),
            ("trimEnd", "stringTrimEnd"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.convert".into(),
        vec![
            "toString",
            "toInt",
            "toFloat",
            "toBool",
            "parseInt",
            "parseFloat",
        ]
        .into_iter()
        .map(|n| (n.into(), n.into()))
        .collect(),
    );

    m.insert(
        "std.dictionary".into(),
        vec![
            "get",
            "getString",
            "getInt",
            "getBool",
            "set",
            "getKeys",
            "hasKey",
        ]
        .into_iter()
        .map(|n| (n.into(), n.into()))
        .collect(),
    );

    m.insert(
        "std.error".into(),
        vec!["Error", "message", "kind", "isError", "unwrap"]
            .into_iter()
            .map(|n| (n.into(), n.into()))
            .collect(),
    );

    m.insert(
        "std.io".into(),
        vec![("print", "print")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect(),
    );

    m.insert(
        "std.json".into(),
        vec![
            ("parse", "jsonParse"),
            ("stringify", "jsonStringify"),
            ("requireString", "jsonRequireString"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.html".into(),
        vec![("escape", "htmlEscape")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect(),
    );

    m.insert(
        "std.browser".into(),
        vec![
            ("setHtml", "setHtml"),
            ("setHtmlAt", "setHtmlAt"),
            ("getLocationPath", "getLocationPath"),
            ("pushHistoryState", "pushHistoryState"),
            ("remoteCall", "remoteCall"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.file".into(),
        vec![
            ("read", "fileRead"),
            ("write", "fileWrite"),
            ("exists", "fileExists"),
            ("list", "fileList"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.process".into(),
        vec![
            ("run", "processRun"),
            ("start", "processStart"),
            ("write", "processWrite"),
            ("read", "processRead"),
            ("stop", "processStop"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.path".into(),
        vec![
            ("join", "pathJoin"),
            ("dirname", "pathDirname"),
            ("basename", "pathBasename"),
            ("extname", "pathExtname"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.env".into(),
        vec![("get", "envGet"), ("load", "envLoad")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect(),
    );

    m.insert(
        "std.events".into(),
        vec![
            ("on", "eventOn"),
            ("once", "eventOnce"),
            ("off", "eventOff"),
            ("emit", "eventEmit"),
            ("subscribers", "eventSubscribers"),
            ("clear", "eventClear"),
            ("clearAll", "eventClearAll"),
            ("emitDeferred", "eventEmitDeferred"),
            ("drain", "eventDrain"),
            ("queueLen", "eventQueueLen"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.time".into(),
        vec![("now", "timeNow"), ("unix", "timeUnix")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect(),
    );

    m.insert(
        "std.storage".into(),
        vec![
            ("storageGet", "storageGet"),
            ("storageSet", "storageSet"),
            ("storageRemove", "storageRemove"),
            ("storageClear", "storageClear"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.log".into(),
        vec![
            ("info", "logInfo"),
            ("warn", "logWarn"),
            ("error", "logError"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.cli".into(),
        vec![
            ("readLine", "cliReadLine"),
            ("write", "cliWrite"),
            ("writeLine", "cliWriteLine"),
            ("clear", "cliClear"),
            ("moveTo", "cliMoveTo"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.test".into(),
        vec![("assert", "testAssert"), ("equal", "testEqual")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect(),
    );

    m.insert(
        "std.http.request".into(),
        vec![
            ("get", "httpRequestGet"),
            ("post", "httpRequestPost"),
            ("put", "httpRequestPut"),
            ("patch", "httpRequestPatch"),
            ("delete", "httpRequestDelete"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.http.server".into(),
        vec![
            ("ok", "httpServerOk"),
            ("text", "httpServerText"),
            ("html", "httpServerHtml"),
            ("json", "httpServerJson"),
            ("redirect", "httpServerRedirect"),
            // Router API
            ("router", "httpServerRouter"),
            ("get", "httpServerRouterGet"),
            ("post", "httpServerRouterPost"),
            ("serveFiles", "httpServerRouterServeFiles"),
            ("listen", "httpServerRouterListen"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.ffi".into(),
        vec![("available", "ffiAvailable")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect(),
    );

    m.insert(
        "std.net".into(),
        vec![("available", "netAvailable")]
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect(),
    );

    m.insert(
        "std.crypto".into(),
        vec![
            ("available", "cryptoAvailable"),
            ("hmacSha256Hex", "cryptoHmacSha256Hex"),
            ("sha256Hex", "cryptoSha256Hex"),
            ("hexEncode", "cryptoHexEncode"),
            ("constantTimeEquals", "cryptoConstantTimeEquals"),
            ("base64Encode", "cryptoBase64Encode"),
            ("base64Decode", "cryptoBase64Decode"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.net.tcp".into(),
        vec![
            ("listen", "netTcpListen"),
            ("accept", "netTcpAccept"),
            ("connect", "netTcpConnect"),
            ("read", "netTcpRead"),
            ("readLine", "netTcpReadLine"),
            ("write", "netTcpWrite"),
            ("close", "netTcpClose"),
            ("address", "netTcpAddress"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m.insert(
        "std.net.udp".into(),
        vec![
            ("bind", "netUdpBind"),
            ("send", "netUdpSend"),
            ("receive", "netUdpReceive"),
            ("close", "netUdpClose"),
            ("broadcast", "netUdpBroadcast"),
        ]
        .into_iter()
        .map(|(a, b)| (a.into(), b.into()))
        .collect(),
    );

    m
}

/// Check if a module path starts with "std".
pub fn is_std_module(path: &[String]) -> bool {
    path.first().map(|s| s.as_str()) == Some("std")
}

/// Convert a module path like ["std", "array"] to "std.array".
pub fn std_module_name(path: &[String]) -> String {
    path.join(".")
}
