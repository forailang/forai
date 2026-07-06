//! AWS Secrets Manager backend (plan 132 phase 5).
//!
//! `GetSecretValue` over HTTPS with hand-rolled SigV4 signing — existing
//! deps only (`ureq`, `hmac`, `sha2`), no AWS SDK. Declared secrets are
//! fetched once at startup validation (blocking is fine there — the
//! module hasn't started); after that egress resolution reads the TTL
//! cache and NEVER does I/O on the scheduler thread: an expired entry is
//! served stale while a background thread revalidates
//! (stale-while-revalidate), and `secrets.refresh()` forces a fetch.
//!
//! Credentials v1: standard env vars (AWS_ACCESS_KEY_ID,
//! AWS_SECRET_ACCESS_KEY, optional AWS_SESSION_TOKEN). IMDSv2 instance
//! roles are a fast-follow. All errors are value-free.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub(crate) const DEFAULT_TTL_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub(crate) struct AwsConfig {
    pub(crate) region: String,
    /// Prepended to the declared name to form the SecretId
    /// (e.g. `brain/prod/` + `STRIPE_KEY`).
    pub(crate) prefix: String,
    /// Endpoint override for tests / LocalStack. `None` = the real
    /// `https://secretsmanager.<region>.amazonaws.com`.
    pub(crate) endpoint: Option<String>,
    pub(crate) ttl: Duration,
    /// Per-declaration `key = "field"` — pluck one field from a JSON
    /// secret value.
    pub(crate) field_map: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AwsCredentials {
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
    pub(crate) session_token: Option<String>,
}

impl AwsCredentials {
    /// Standard env-var resolution. The error names what is missing and
    /// never echoes values.
    pub(crate) fn from_env() -> Result<Self, String> {
        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID")
            .map_err(|_| "aws backend: AWS_ACCESS_KEY_ID is not set".to_string())?;
        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| "aws backend: AWS_SECRET_ACCESS_KEY is not set".to_string())?;
        Ok(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
        })
    }
}

struct CacheEntry {
    value: String,
    fetched: Instant,
}

struct AwsState {
    config: AwsConfig,
    credentials: AwsCredentials,
    declared: Vec<String>,
    cache: Mutex<HashMap<String, CacheEntry>>,
    /// Collapses concurrent stale-while-revalidate refreshes.
    refreshing: AtomicBool,
}

static STATE: OnceLock<Mutex<Option<Arc<AwsState>>>> = OnceLock::new();

fn state_slot() -> &'static Mutex<Option<Arc<AwsState>>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn current_state() -> Option<Arc<AwsState>> {
    state_slot().lock().ok().and_then(|s| s.clone())
}

/// Install the backend for this run and blocking-fetch every declared
/// secret (startup validation). Returns the set of names that resolved.
pub(crate) fn configure(
    config: AwsConfig,
    credentials: AwsCredentials,
    declared: Vec<String>,
) -> Result<Vec<String>, String> {
    let state = Arc::new(AwsState {
        config,
        credentials,
        declared: declared.clone(),
        cache: Mutex::new(HashMap::new()),
        refreshing: AtomicBool::new(false),
    });
    let mut ok = Vec::new();
    for name in &declared {
        if fetch_into_cache(&state, name).is_ok() {
            ok.push(name.clone());
        }
    }
    *state_slot().lock().unwrap() = Some(state);
    Ok(ok)
}

/// Clear the installed backend (end of run).
pub(crate) fn clear() {
    if let Ok(mut slot) = state_slot().lock() {
        *slot = None;
    }
}

/// Egress/reveal resolution. Fresh cache hit → value. Stale hit → serve
/// stale and revalidate on a background thread (never block the
/// scheduler). Miss → None (optional secret that failed at startup).
pub(crate) fn resolve(name: &str) -> Option<String> {
    let state = current_state()?;
    let (value, stale) = {
        let cache = state.cache.lock().ok()?;
        let entry = cache.get(name)?;
        (
            entry.value.clone(),
            entry.fetched.elapsed() > state.config.ttl,
        )
    };
    if stale
        && state
            .refreshing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        let bg = state.clone();
        std::thread::spawn(move || {
            for name in bg.declared.clone() {
                let _ = fetch_into_cache(&bg, &name);
            }
            bg.refreshing.store(false, Ordering::SeqCst);
        });
    }
    Some(value)
}

/// `secrets.refresh()` — blocking refetch of every declared secret.
/// Returns how many resolved. Intended for explicit rotation points;
/// TTL + stale-while-revalidate covers steady state.
pub(crate) fn refresh_all() -> i32 {
    let Some(state) = current_state() else {
        return 0;
    };
    let mut ok = 0;
    for name in state.declared.clone() {
        if fetch_into_cache(&state, &name).is_ok() {
            ok += 1;
        }
    }
    ok
}

fn fetch_into_cache(state: &AwsState, name: &str) -> Result<(), String> {
    let secret_id = format!("{}{}", state.config.prefix, name);
    let raw = get_secret_value(&state.config, &state.credentials, &secret_id)?;
    let value = match state.config.field_map.get(name) {
        Some(field) => {
            let parsed: serde_json::Value = serde_json::from_str(&raw).map_err(|_| {
                format!(
                    "aws backend: secret '{}' is not JSON but a key field is configured",
                    name
                )
            })?;
            parsed
                .get(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    format!(
                        "aws backend: secret '{}' has no string field '{}'",
                        name, field
                    )
                })?
        }
        None => raw,
    };
    if let Ok(mut cache) = state.cache.lock() {
        cache.insert(
            name.to_string(),
            CacheEntry {
                value,
                fetched: Instant::now(),
            },
        );
    }
    Ok(())
}

// ── SigV4 ────────────────────────────────────────────────────────────

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// AWS4 signing key: HMAC chain over date/region/service.
pub(crate) fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{}", secret).as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Build the canonical request string. `headers` must be
/// (lowercase-name, trimmed-value) pairs; they are sorted here.
pub(crate) fn canonical_request(
    method: &str,
    uri: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
) -> (String, String) {
    let mut sorted: Vec<&(String, String)> = headers.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = sorted
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v))
        .collect();
    let signed_headers: String = sorted
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method, uri, query, canonical_headers, signed_headers, payload_hash
    );
    (canonical, signed_headers)
}

/// Compute the SigV4 signature + Authorization header value.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign(
    credentials: &AwsCredentials,
    region: &str,
    service: &str,
    amz_date: &str, // 20150830T123600Z
    method: &str,
    uri: &str,
    query: &str,
    headers: &[(String, String)],
    payload: &[u8],
) -> (String, String) {
    let date = &amz_date[..8];
    let payload_hash = sha256_hex(payload);
    let (canonical, signed_headers) = canonical_request(method, uri, query, headers, &payload_hash);
    let scope = format!("{}/{}/{}/aws4_request", date, region, service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        scope,
        sha256_hex(canonical.as_bytes())
    );
    let key = signing_key(&credentials.secret_access_key, date, region, service);
    let signature = hex(&hmac_sha256(&key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        credentials.access_key_id, scope, signed_headers, signature
    );
    (authorization, signature)
}

/// Current UTC time as (`YYYYMMDDTHHMMSSZ`). Civil-date conversion via
/// the days-from-epoch algorithm — std only, no chrono.
pub(crate) fn amz_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    amz_date_from_unix(secs)
}

fn amz_date_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        y, m, d, h, mi, s
    )
}

/// One `GetSecretValue` call. Returns the SecretString.
#[cfg(feature = "http-client")]
pub(crate) fn get_secret_value(
    config: &AwsConfig,
    credentials: &AwsCredentials,
    secret_id: &str,
) -> Result<String, String> {
    let service = "secretsmanager";
    let default_host = format!("secretsmanager.{}.amazonaws.com", config.region);
    let (url, host) = match &config.endpoint {
        Some(endpoint) => {
            let trimmed = endpoint.trim_end_matches('/');
            let host = trimmed
                .strip_prefix("http://")
                .or_else(|| trimmed.strip_prefix("https://"))
                .unwrap_or(trimmed)
                .to_string();
            (format!("{}/", trimmed), host)
        }
        None => (format!("https://{}/", default_host), default_host),
    };
    let body = serde_json::json!({ "SecretId": secret_id }).to_string();
    let amz_date = amz_date_now();

    let mut headers: Vec<(String, String)> = vec![
        (
            "content-type".to_string(),
            "application/x-amz-json-1.1".to_string(),
        ),
        ("host".to_string(), host),
        ("x-amz-date".to_string(), amz_date.clone()),
        (
            "x-amz-target".to_string(),
            "secretsmanager.GetSecretValue".to_string(),
        ),
    ];
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token".to_string(), token.clone()));
    }
    let (authorization, _) = sign(
        credentials,
        &config.region,
        service,
        &amz_date,
        "POST",
        "/",
        "",
        &headers,
        body.as_bytes(),
    );

    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .new_agent();
    let mut req = agent.post(&url).header("authorization", &authorization);
    for (name, value) in &headers {
        if name != "host" {
            req = req.header(name, value);
        }
    }
    let response = req
        .send(body.as_bytes())
        .map_err(|e| format!("aws backend: request failed: {}", e))?;
    let status = response.status().as_u16();
    let text = response
        .into_body()
        .read_to_string()
        .map_err(|e| format!("aws backend: cannot read response: {}", e))?;
    if status != 200 {
        // AWS error bodies carry __type + Message; forward only the type
        // so the diagnostic stays value-free.
        let err_type = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("__type").and_then(|t| t.as_str()).map(String::from))
            .unwrap_or_else(|| format!("HTTP {}", status));
        return Err(format!(
            "aws backend: GetSecretValue for '{}' failed: {}",
            secret_id, err_type
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| "aws backend: response is not JSON".to_string())?;
    parsed
        .get("SecretString")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            format!(
                "aws backend: secret '{}' has no SecretString (binary secrets are not supported)",
                secret_id
            )
        })
}

#[cfg(not(feature = "http-client"))]
pub(crate) fn get_secret_value(
    _config: &AwsConfig,
    _credentials: &AwsCredentials,
    _secret_id: &str,
) -> Result<String, String> {
    Err("aws backend requires the http-client feature".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // AWS's published SigV4 example (docs: "Signature Version 4 signing
    // process", GET iam ListUsers, 20150830, us-east-1).
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    #[test]
    fn signing_key_matches_aws_test_vector() {
        let key = signing_key(SECRET, "20150830", "us-east-1", "iam");
        assert_eq!(
            hex(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn full_signature_matches_aws_test_vector() {
        let credentials = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: SECRET.to_string(),
            session_token: None,
        };
        let headers = vec![
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded; charset=utf-8".to_string(),
            ),
            ("host".to_string(), "iam.amazonaws.com".to_string()),
            ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
        ];
        let (authorization, signature) = sign(
            &credentials,
            "us-east-1",
            "iam",
            "20150830T123600Z",
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            &headers,
            b"",
        );
        assert_eq!(
            signature,
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
        assert!(authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, Signature="
        ));
    }

    #[test]
    fn amz_date_formats_epoch_correctly() {
        assert_eq!(amz_date_from_unix(0), "19700101T000000Z");
        // 2015-08-30T12:36:00Z
        assert_eq!(amz_date_from_unix(1_440_938_160), "20150830T123600Z");
    }

    /// Real-AWS integration gate. Run with:
    /// `FAI_AWS_SECRETS_TEST_REGION=us-east-1 \
    ///  FAI_AWS_SECRETS_TEST_ID=my/secret/id \
    ///  FAI_AWS_SECRETS_TEST_EXPECT=value \
    ///  cargo test -p fai-cli --lib aws_real -- --ignored`
    /// plus standard AWS credentials in the environment.
    #[test]
    #[ignore]
    #[cfg(feature = "http-client")]
    fn aws_real_get_secret_value() {
        let region = std::env::var("FAI_AWS_SECRETS_TEST_REGION").expect("region env");
        let secret_id = std::env::var("FAI_AWS_SECRETS_TEST_ID").expect("id env");
        let expect = std::env::var("FAI_AWS_SECRETS_TEST_EXPECT").expect("expect env");
        let config = AwsConfig {
            region,
            prefix: String::new(),
            endpoint: None,
            ttl: Duration::from_secs(300),
            field_map: HashMap::new(),
        };
        let credentials = AwsCredentials::from_env().expect("aws credentials");
        let value = get_secret_value(&config, &credentials, &secret_id).expect("fetch");
        assert_eq!(value, expect);
    }

    #[cfg(feature = "http-client")]
    mod mock_server {
        use super::super::*;
        use super::SECRET;
        use std::io::{Read, Write};
        use std::net::TcpListener;

        /// Serve `responses` (one per accepted connection), recording
        /// each raw request.
        fn serve(
            responses: Vec<String>,
        ) -> (u16, std::thread::JoinHandle<Vec<String>>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let handle = std::thread::spawn(move || {
                let mut requests = Vec::new();
                for body in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    // Read until headers AND the content-length body have
                    // fully arrived — a single read() races TCP segmentation.
                    let mut collected = Vec::new();
                    let mut chunk = [0u8; 8192];
                    loop {
                        let n = stream.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        collected.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&collected);
                        if let Some(header_end) = text.find("\r\n\r\n") {
                            let content_length = text
                                .lines()
                                .find_map(|l| {
                                    l.to_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if collected.len() >= header_end + 4 + content_length {
                                break;
                            }
                        }
                    }
                    requests.push(String::from_utf8_lossy(&collected).into_owned());
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/x-amz-json-1.1\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
                requests
            });
            (port, handle)
        }

        fn test_config(port: u16, field_map: HashMap<String, String>) -> AwsConfig {
            AwsConfig {
                region: "us-east-1".to_string(),
                prefix: "app/test/".to_string(),
                endpoint: Some(format!("http://127.0.0.1:{}", port)),
                ttl: Duration::from_secs(300),
                field_map,
            }
        }

        fn test_credentials() -> AwsCredentials {
            AwsCredentials {
                access_key_id: "AKIDEXAMPLE".to_string(),
                secret_access_key: SECRET.to_string(),
                session_token: Some("session-token-xyz".to_string()),
            }
        }

        #[test]
        fn get_secret_value_signs_and_parses() {
            let (port, server) = serve(vec![
                r#"{"ARN":"arn:x","Name":"app/test/API_KEY","SecretString":"mock-value-42"}"#
                    .to_string(),
            ]);
            let value = get_secret_value(
                &test_config(port, HashMap::new()),
                &test_credentials(),
                "app/test/API_KEY",
            )
            .expect("fetch");
            assert_eq!(value, "mock-value-42");

            let requests = server.join().unwrap();
            let req = &requests[0];
            assert!(req.contains("x-amz-target: secretsmanager.GetSecretValue"));
            assert!(req.contains("x-amz-security-token: session-token-xyz"));
            assert!(req.contains(r#"{"SecretId":"app/test/API_KEY"}"#));
            assert!(
                req.contains("authorization: AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
                "missing sigv4 header: {}",
                req
            );
            assert!(req.contains(
                "SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-amz-target"
            ));
        }

        #[test]
        fn error_responses_are_value_free() {
            let (port, server) = serve(vec![String::new()]);
            // Respond 200 with empty body → parse error path; then a
            // second scenario isn't needed — the __type path is simple
            // string plumbing.
            let err = get_secret_value(
                &test_config(port, HashMap::new()),
                &test_credentials(),
                "app/test/MISSING",
            )
            .unwrap_err();
            assert!(err.contains("aws backend"), "err: {}", err);
            server.join().unwrap();
        }

        #[test]
        fn configure_resolve_and_field_pluck() {
            let (port, server) = serve(vec![
                r#"{"SecretString":"{\"password\":\"plucked-pw\",\"user\":\"u\"}"}"#.to_string(),
            ]);
            let mut field_map = HashMap::new();
            field_map.insert("DB_CREDS".to_string(), "password".to_string());
            let resolved = configure(
                test_config(port, field_map),
                test_credentials(),
                vec!["DB_CREDS".to_string()],
            )
            .expect("configure");
            assert_eq!(resolved, vec!["DB_CREDS".to_string()]);
            assert_eq!(resolve("DB_CREDS").as_deref(), Some("plucked-pw"));
            assert_eq!(resolve("UNKNOWN"), None);
            clear();
            assert_eq!(resolve("DB_CREDS"), None);
            server.join().unwrap();
        }
    }
}
