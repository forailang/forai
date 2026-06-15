//! Crypto host imports: SHA-256, HMAC-SHA256, HMAC-SHA1, hex, base64, and a
//! constant-time string compare. All native-only; the browser target
//! strips every function except `crypto_available` (which reports false
//! there). String args arrive as (ptr, len) into guest memory; string
//! results are allocated back onto the guest heap as NaN-boxed strings.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use wasmtime::*;

use super::super::heap::wasm_alloc_str;

type HmacSha256 = Hmac<Sha256>;
type HmacSha1 = Hmac<Sha1>;

/// Read a (ptr, len) guest string into an owned String. Out-of-bounds
/// ranges yield an empty string rather than panicking the host.
fn read_str(caller: &Caller<'_, ()>, mem: &Memory, ptr: i32, len: i32) -> String {
    let data = mem.data(caller);
    let start = ptr as usize;
    let end = start.saturating_add(len.max(0) as usize);
    if start > data.len() || end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

/// Lowercase hex encoding of a byte slice.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub(super) fn install(linker: &mut Linker<()>) -> Result<(), String> {
    // env.crypto_available() -> i32 (1/0). Native always reports true; the
    // browser linker stubs this to 0.
    linker
        .func_wrap(
            "env",
            "crypto_available",
            |_caller: Caller<'_, ()>| -> i32 { 1 },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.crypto_hmac_sha256_hex(key_ptr, key_len, msg_ptr, msg_len) -> i64.
    linker
        .func_wrap(
            "env",
            "crypto_hmac_sha256_hex",
            |mut caller: Caller<'_, ()>,
             key_ptr: i32,
             key_len: i32,
             msg_ptr: i32,
             msg_len: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let key = read_str(&caller, &mem, key_ptr, key_len);
                let msg = read_str(&caller, &mem, msg_ptr, msg_len);
                // HMAC accepts a key of any length.
                let mut mac = HmacSha256::new_from_slice(key.as_bytes()).unwrap();
                mac.update(msg.as_bytes());
                let digest = mac.finalize().into_bytes();
                wasm_alloc_str(&mut caller, &mem, &to_hex(&digest))
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.crypto_hmac_sha1_base64(key_ptr, key_len, msg_ptr, msg_len) -> i64.
    linker
        .func_wrap(
            "env",
            "crypto_hmac_sha1_base64",
            |mut caller: Caller<'_, ()>,
             key_ptr: i32,
             key_len: i32,
             msg_ptr: i32,
             msg_len: i32|
             -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let key = read_str(&caller, &mem, key_ptr, key_len);
                let msg = read_str(&caller, &mem, msg_ptr, msg_len);
                let mut mac = HmacSha1::new_from_slice(key.as_bytes()).unwrap();
                mac.update(msg.as_bytes());
                let digest = mac.finalize().into_bytes();
                wasm_alloc_str(&mut caller, &mem, &STANDARD.encode(digest))
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.crypto_sha256_hex(ptr, len) -> i64.
    linker
        .func_wrap(
            "env",
            "crypto_sha256_hex",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = read_str(&caller, &mem, ptr, len);
                let mut hasher = Sha256::new();
                hasher.update(data.as_bytes());
                let digest = hasher.finalize();
                wasm_alloc_str(&mut caller, &mem, &to_hex(&digest))
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.crypto_hex_encode(ptr, len) -> i64.
    linker
        .func_wrap(
            "env",
            "crypto_hex_encode",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = read_str(&caller, &mem, ptr, len);
                wasm_alloc_str(&mut caller, &mem, &to_hex(data.as_bytes()))
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.crypto_constant_time_equals(a_ptr, a_len, b_ptr, b_len) -> i32.
    linker
        .func_wrap(
            "env",
            "crypto_constant_time_equals",
            |mut caller: Caller<'_, ()>, a_ptr: i32, a_len: i32, b_ptr: i32, b_len: i32| -> i32 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let a = read_str(&caller, &mem, a_ptr, a_len);
                let b = read_str(&caller, &mem, b_ptr, b_len);
                if a.len() != b.len() {
                    return 0;
                }
                if bool::from(a.as_bytes().ct_eq(b.as_bytes())) {
                    1
                } else {
                    0
                }
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.crypto_base64_encode(ptr, len) -> i64.
    linker
        .func_wrap(
            "env",
            "crypto_base64_encode",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = read_str(&caller, &mem, ptr, len);
                let encoded = STANDARD.encode(data.as_bytes());
                wasm_alloc_str(&mut caller, &mem, &encoded)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    // env.crypto_base64_decode(ptr, len) -> i64. Decoded bytes are returned
    // as a UTF-8 (lossy) string; invalid base64 yields an empty string.
    linker
        .func_wrap(
            "env",
            "crypto_base64_decode",
            |mut caller: Caller<'_, ()>, ptr: i32, len: i32| -> i64 {
                let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
                let data = read_str(&caller, &mem, ptr, len);
                let decoded = STANDARD.decode(data.as_bytes()).unwrap_or_default();
                let text = String::from_utf8_lossy(&decoded).into_owned();
                wasm_alloc_str(&mut caller, &mem, &text)
            },
        )
        .map_err(|e| format!("linker error: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_answer() {
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        assert_eq!(
            to_hex(&hasher.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_case2() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?".
        let mut mac = HmacSha256::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        assert_eq!(
            to_hex(&mac.finalize().into_bytes()),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_sha1_rfc2202_case2_base64() {
        // RFC 2202 test case 2: key "Jefe", data "what do ya want for nothing?".
        let mut mac = HmacSha1::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        assert_eq!(
            STANDARD.encode(mac.finalize().into_bytes()),
            "7/zfauXrL6LSdBbV8YTfnCWafHk="
        );
    }

    #[test]
    fn base64_round_trip() {
        let encoded = STANDARD.encode(b"hello forai");
        assert_eq!(encoded, "aGVsbG8gZm9yYWk=");
        let decoded = STANDARD.decode(encoded.as_bytes()).unwrap();
        assert_eq!(decoded, b"hello forai");
    }

    #[test]
    fn hex_encode_lowercase() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn slack_signature_vector() {
        // Slack's published signing example.
        let secret = b"8f742231b10e8888abcd99yyyzzz85a5";
        let timestamp = "1531420618";
        let body = "token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
        let basestring = format!("v0:{}:{}", timestamp, body);
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(basestring.as_bytes());
        let sig = format!("v0={}", to_hex(&mac.finalize().into_bytes()));
        assert_eq!(
            sig,
            "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503"
        );
    }
}
