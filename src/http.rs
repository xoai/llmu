use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CACHE_SCHEMA_VERSION: u32 = 1;

/// A non-2xx HTTP status returned by an endpoint. `code` carries the
/// status; `note` is the one-line body snippet the caller saw.
#[derive(Debug)]
pub struct HttpStatusError {
    pub code: u16,
    pub note: String,
}

impl fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {}: {}", self.code, self.note)
    }
}

impl std::error::Error for HttpStatusError {}

/// Classify a failed cost-report fetch for the user-visible notes.
/// A 404 means the endpoint is absent (some org types lack it) and is a
/// silent break; every other failure gets a note so the user knows the
/// billed totals may be incomplete.
pub fn cost_break_note(e: &anyhow::Error, provider: &str) -> Option<String> {
    match e.downcast_ref::<HttpStatusError>() {
        Some(se) if se.code == 404 => None,
        _ => Some(format!(
            "{provider}: cost report failed ({e}) — billed totals may be incomplete"
        )),
    }
}

/// Set LLMU_DEBUG=1 to dump every request URL and a response snippet to
/// stderr — the fast way to diagnose payload drift on the undocumented
/// endpoints (they can change shape without notice). Request bodies are
/// never dumped: OAuth payloads may carry refresh tokens.
fn debug() -> bool {
    std::env::var("LLMU_DEBUG")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn with_headers(mut req: ureq::Request, headers: &[(&str, &str)]) -> ureq::Request {
    for (k, v) in headers {
        req = req.set(k, v);
    }
    req
}

/// Format the LLMU_DEBUG response line. GET keeps an 800-char body snippet
/// for payload-drift diagnosis (existing behavior); POST never prints the
/// response body — OAuth/refresh responses can carry access tokens, and
/// secrets must never reach diagnostics (NFR Security). `status` is the
/// status suffix ("200", "500", ...). Pure: the policy is testable without
/// touching the process environment or capturing stderr.
fn debug_snippet(method: &str, url: &str, status: &str, body: &str) -> String {
    if method == "POST" {
        return format!(
            "[debug] {method} {url}\n[debug] {status}: response body suppressed (may contain credentials)"
        );
    }
    let snip: String = body.chars().take(800).collect();
    format!("[debug] {method} {url}\n[debug] {status}: {snip}")
}

/// Shared response mapping for GET and POST: parse a 2xx body as JSON,
/// surface non-2xx as `HttpStatusError` with a one-line body snippet, and
/// keep the LLMU_DEBUG dump on stderr. `method` feeds the debug line and
/// its body policy (POST bodies are suppressed).
fn finish(
    resp: Result<ureq::Response, ureq::Error>,
    method: &str,
    url: &str,
) -> Result<serde_json::Value> {
    match resp {
        Ok(r) => {
            let body = r.into_string()?;
            if debug() {
                eprintln!("{}", debug_snippet(method, url, "200", &body));
            }
            Ok(serde_json::from_str(&body)?)
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            if debug() {
                eprintln!("{}", debug_snippet(method, url, &code.to_string(), &body));
            }
            // Notes stay one line: collapse whitespace, cap length.
            let compact: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let snippet: String = compact.chars().take(200).collect();
            Err(anyhow::Error::new(HttpStatusError {
                code,
                note: snippet,
            }))
        }
        Err(e) => {
            if debug() {
                eprintln!("[debug] {method} {url}\n[debug] transport error: {e}");
            }
            Err(e.into())
        }
    }
}

pub fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<serde_json::Value> {
    let req = with_headers(ureq::get(url).timeout(REQUEST_TIMEOUT), headers);
    finish(req.call(), "GET", url)
}

// ---------------------------------------------------------------------------
// Optional raw HTTP TTL cache (FR-3, AD-1)
// ---------------------------------------------------------------------------

/// Where a cache-aware GET result came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOrigin {
    /// Response came from the network in this call.
    Live,
    /// Response was replayed from the raw HTTP TTL cache.
    Cached,
}

/// JSON body plus cache provenance: `observed_at_ms` is the original
/// observation time — network response time on a live fetch, or the stored
/// value on a cache hit — so callers never mistake a replayed hit for fresh
/// data (FR-3.10).
#[derive(Debug, Clone)]
pub struct CachedJson {
    pub body: serde_json::Value,
    pub origin: CacheOrigin,
    /// Original observation time (ms since epoch): the network response time
    /// on a live fetch, the stored value on a cache hit. Kept public because
    /// it is the cache-provenance surface consumers use to distinguish a
    /// replayed hit from fresh data (FR-3.10); current providers consume
    /// `CacheOrigin`, so this field stays for that public surface.
    #[allow(dead_code)]
    pub observed_at_ms: u64,
}

/// Per-call cache options. Zero `ttl_seconds` (the default) disables cache
/// reads and writes without changing live behavior (FR-3.1). `dir` None
/// resolves to the platform cache directory under `llmu/http` (FR-3.4);
/// tests pass a temp dir.
#[derive(Debug, Clone, Default)]
pub struct CacheOptions {
    pub dir: Option<PathBuf>,
    pub ttl_seconds: u64,
}

impl CacheOptions {
    pub fn enabled(&self) -> bool {
        self.ttl_seconds > 0
    }

    fn resolve_dir(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(d) => Ok(d.clone()),
            None => dirs::cache_dir()
                .map(|c| c.join("llmu/http"))
                .ok_or_else(|| anyhow::anyhow!("no platform cache directory")),
        }
    }
}

fn has_crlf(s: &str) -> bool {
    s.as_bytes().contains(&b'\r') || s.as_bytes().contains(&b'\n')
}

/// Canonical request bytes for the cache key (FR-3.5): uppercase method, LF,
/// URL bytes, LF, then every `(name, value)` header pair with the ASCII name
/// lowercased, sorted by `(name, value)`, encoded as `name:value` plus LF.
/// Values are used verbatim. CR/LF anywhere in method, URL, name, or value
/// makes the request cache-ineligible (None) — it can never be forged into
/// another entry's filename.
fn canonical_cache_bytes(method: &str, url: &str, headers: &[(&str, &str)]) -> Option<Vec<u8>> {
    if has_crlf(method) || has_crlf(url) {
        return None;
    }
    let mut pairs: Vec<(String, &str)> = headers
        .iter()
        .map(|(n, v)| (n.to_ascii_lowercase(), *v))
        .collect();
    if pairs.iter().any(|(n, v)| has_crlf(n) || has_crlf(v)) {
        return None;
    }
    pairs.sort();
    let mut out = Vec::new();
    out.extend_from_slice(method.to_uppercase().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(url.as_bytes());
    out.push(b'\n');
    for (n, v) in &pairs {
        out.extend_from_slice(n.as_bytes());
        out.push(b':');
        out.extend_from_slice(v.as_bytes());
        out.push(b'\n');
    }
    Some(out)
}

/// Lowercase SHA-256 hex cache filename for a request (FR-3.5). None when the
/// request is cache-ineligible. Raw URLs and header values never reach the
/// filename or envelope metadata.
pub fn cache_key_sha256(method: &str, url: &str, headers: &[(&str, &str)]) -> Option<String> {
    use sha2::{Digest, Sha256};
    canonical_cache_bytes(method, url, headers).map(|b| format!("{:x}", Sha256::digest(&b)))
}

/// On-disk envelope (FR-3.6): schema version, original observation time, and
/// the raw JSON body. No request secrets are ever stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEnvelope {
    version: u32,
    observed_at_ms: u64,
    body: serde_json::Value,
}

/// Read a valid, unexpired, non-future-dated entry. Ok(None) is a miss;
/// Err is a corrupt/unreadable cache (falls back live with a note).
fn read_entry(dir: &Path, key: &str, ttl_seconds: u64, now: u64) -> Result<Option<CacheEnvelope>> {
    let path = dir.join(key);
    let raw = match fs::read(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let env: CacheEnvelope = serde_json::from_slice(&raw).context("unreadable cache entry")?;
    if env.version != CACHE_SCHEMA_VERSION {
        bail!("unsupported cache entry version {}", env.version);
    }
    let ttl_ms = ttl_seconds.saturating_mul(1000);
    if env.observed_at_ms > now || env.observed_at_ms.saturating_add(ttl_ms) <= now {
        return Ok(None);
    }
    Ok(Some(env))
}

/// Atomic restrictive write (FR-3.4, FR-3.7): same-directory unique temporary
/// file, restrictive permissions, flush, then rename over the target.
fn write_entry(dir: &Path, key: &str, observed: u64, body: &serde_json::Value) -> Result<()> {
    fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)?;
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{key}.{}.{nonce}.tmp", std::process::id()));
    let data = serde_json::to_vec(&CacheEnvelope {
        version: CACHE_SCHEMA_VERSION,
        observed_at_ms: observed,
        body: body.clone(),
    })?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    set_mode(&tmp, 0o600)?;
    use std::io::Write;
    f.write_all(&data)?;
    f.sync_all()?;
    fs::rename(&tmp, dir.join(key))?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_p: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// One-line secret-free stderr note; cache trouble never fails a live
/// response (FR-3.8).
fn note_cache_failure(e: &anyhow::Error) {
    eprintln!("llmu: http cache note: {e} — falling back to a live request");
}

/// Cache-aware GET (FR-3, AD-1). With caching enabled, consults the TTL cache
/// before the network unless `fresh`; a successful live response is stored
/// (also when `fresh`, FR-3.2). Failed responses are never cached (FR-3.7).
/// Corrupt/read/write failures emit a note and fall back live (FR-3.8).
pub fn get_json_cached(
    opts: &CacheOptions,
    fresh: bool,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<CachedJson> {
    if !opts.enabled() {
        let observed = now_ms();
        let body = get_json(url, headers)?;
        return Ok(CachedJson {
            body,
            origin: CacheOrigin::Live,
            observed_at_ms: observed,
        });
    }
    let key = match cache_key_sha256("GET", url, headers) {
        Some(k) => k,
        None => {
            // Cache-ineligible (CR/LF); behave exactly like a live GET.
            let observed = now_ms();
            let body = get_json(url, headers)?;
            return Ok(CachedJson {
                body,
                origin: CacheOrigin::Live,
                observed_at_ms: observed,
            });
        }
    };
    let dir = match opts.resolve_dir() {
        Ok(d) => d,
        Err(e) => {
            note_cache_failure(&e);
            let observed = now_ms();
            let body = get_json(url, headers)?;
            return Ok(CachedJson {
                body,
                origin: CacheOrigin::Live,
                observed_at_ms: observed,
            });
        }
    };
    if !fresh {
        match read_entry(&dir, &key, opts.ttl_seconds, now_ms()) {
            Ok(Some(env)) => {
                return Ok(CachedJson {
                    body: env.body,
                    origin: CacheOrigin::Cached,
                    observed_at_ms: env.observed_at_ms,
                })
            }
            Ok(None) => {}
            Err(e) => note_cache_failure(&e),
        }
    }
    let observed = now_ms();
    match get_json(url, headers) {
        Ok(body) => {
            if let Err(e) = write_entry(&dir, &key, observed, &body) {
                note_cache_failure(&e);
            }
            Ok(CachedJson {
                body,
                origin: CacheOrigin::Live,
                observed_at_ms: observed,
            })
        }
        Err(e) => Err(e),
    }
}

/// Uncacheable JSON POST (AD-4): 30s timeout, `HttpStatusError`, and the same
/// secret-free debug policy as GET. OAuth token exchanges and Gemini quota
/// RPCs are never cached (FR-3.3).
pub fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    let req = with_headers(ureq::post(url).timeout(REQUEST_TIMEOUT), headers);
    finish(req.send_json(body), "POST", url)
}

/// Uncacheable form-urlencoded POST (AD-4): used by OAuth token refresh.
pub fn post_form_json(
    url: &str,
    headers: &[(&str, &str)],
    form: &[(&str, &str)],
) -> Result<serde_json::Value> {
    let req = with_headers(ureq::post(url).timeout(REQUEST_TIMEOUT), headers);
    finish(req.send_form(form), "POST", url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Pinned canonical bytes (FR-3.5):
    /// `GET\nhttps://api.example.test/v1/quota\naccept:application/json\n
    /// authorization:Bearer abc123\nuser-agent:llmu-test/1.0\n`
    const EXPECTED_CANONICAL_HEX: &str =
        "f537df58be1451879b8866c194f1d192aa00df5562b0db5618314883b5d93e71";

    fn status_err(code: u16) -> anyhow::Error {
        anyhow::Error::new(HttpStatusError {
            code,
            note: "no note needed".into(),
        })
    }

    static TEST_DIR_NONCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = TEST_DIR_NONCE.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "llmu-http-test-{}-{}-{nonce}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Local HTTP fixture: serves one response per expected request and
    /// counts connections, so a cache hit can be proven network-free.
    struct CounterServer {
        url: String,
        hits: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl CounterServer {
        fn start(responses: Vec<(u16, &'static str)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
            let (h2, r2) = (hits.clone(), requests.clone());
            thread::spawn(move || {
                for (code, body) in responses {
                    let (mut sock, _) = listener.accept().unwrap();
                    let mut buf: Vec<u8> = vec![];
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = sock.read(&mut tmp).unwrap();
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
                            let cl = head.lines().find_map(|l| {
                                l.strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            });
                            if let Some(cl) = cl {
                                let header_end =
                                    buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                                while buf.len() < header_end + cl {
                                    let n = sock.read(&mut tmp).unwrap();
                                    if n == 0 {
                                        break;
                                    }
                                    buf.extend_from_slice(&tmp[..n]);
                                }
                            }
                            break;
                        }
                    }
                    r2.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf).into_owned());
                    h2.fetch_add(1, Ordering::SeqCst);
                    let reason = match code {
                        200 => "OK",
                        400 => "Bad Request",
                        401 => "Unauthorized",
                        404 => "Not Found",
                        500 => "Internal Server Error",
                        _ => "X",
                    };
                    let resp = format!(
                        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    sock.write_all(resp.as_bytes()).unwrap();
                }
            });
            Self {
                url: format!("http://{addr}/v1/usage"),
                hits,
                requests,
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    // -------------------------------------------------------------------
    // cache key canonicalization (FR-3.5)
    // -------------------------------------------------------------------

    #[test]
    fn cache_key_is_lowercase_sha256_of_canonical_request_bytes() {
        let headers = [
            ("Authorization", "Bearer abc123"),
            ("Accept", "application/json"),
            ("User-Agent", "llmu-test/1.0"),
        ];
        let key = cache_key_sha256("GET", "https://api.example.test/v1/quota", &headers)
            .expect("eligible request");
        assert_eq!(key.len(), 64);
        assert!(
            key.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "filename digest must be lowercase hex"
        );
        assert_eq!(
            key, EXPECTED_CANONICAL_HEX,
            "canonical bytes: uppercase method, LF, URL, LF, lowercased sorted name:value pairs + LF"
        );
    }

    #[test]
    fn cache_key_ignores_header_order_and_name_case() {
        let a = cache_key_sha256(
            "GET",
            "https://x.test/q",
            &[("X-API-Key", "k"), ("Accept", "a")],
        )
        .unwrap();
        let b = cache_key_sha256(
            "GET",
            "https://x.test/q",
            &[("accept", "a"), ("x-api-key", "k")],
        )
        .unwrap();
        assert_eq!(a, b, "sorted lowercased pairs must canonicalize");
        let c = cache_key_sha256(
            "GET",
            "https://x.test/q",
            &[("Accept", "a"), ("X-API-Key", "k")],
        )
        .unwrap();
        assert_eq!(a, c);
    }

    #[test]
    fn cache_key_binds_header_values_verbatim() {
        let a = cache_key_sha256(
            "GET",
            "https://x.test/q",
            &[("Authorization", "Bearer one")],
        )
        .unwrap();
        let b = cache_key_sha256(
            "GET",
            "https://x.test/q",
            &[("Authorization", "Bearer two")],
        )
        .unwrap();
        assert_ne!(
            a, b,
            "authorization changes must produce a different key (FR-3.9)"
        );
    }

    #[test]
    fn cache_key_differs_by_url_and_method() {
        let url_a = cache_key_sha256("GET", "https://x.test/a", &[]).unwrap();
        let url_b = cache_key_sha256("GET", "https://x.test/b", &[]).unwrap();
        assert_ne!(url_a, url_b);
        let post = cache_key_sha256("POST", "https://x.test/a", &[]).unwrap();
        assert_ne!(url_a, post, "method participates in the canonical bytes");
    }

    #[test]
    fn crlf_in_url_or_header_makes_request_cache_ineligible() {
        assert!(cache_key_sha256("GET", "https://x.test/a\nb", &[]).is_none());
        assert!(cache_key_sha256("GET", "https://x.test/a", &[("X-N", "a\rb")]).is_none());
        assert!(cache_key_sha256("GET", "https://x.test/a", &[("X-N", "a\nb")]).is_none());
        assert!(cache_key_sha256("GET\r\n", "https://x.test/a", &[]).is_none());
    }

    // -------------------------------------------------------------------
    // cache behavior against a counting listener (AS-7)
    // -------------------------------------------------------------------

    #[test]
    fn same_auth_within_ttl_serves_cache_hit() {
        let srv = CounterServer::start(vec![(200, r#"{"quota":{"used":1}}"#)]);
        let opts = CacheOptions {
            dir: Some(temp_dir("ttl-hit")),
            ttl_seconds: 3600,
        };
        let one =
            get_json_cached(&opts, false, &srv.url, &[("Authorization", "Bearer A")]).unwrap();
        assert_eq!(one.origin, CacheOrigin::Live);
        assert_eq!(srv.hits(), 1);
        let two =
            get_json_cached(&opts, false, &srv.url, &[("Authorization", "Bearer A")]).unwrap();
        assert_eq!(two.origin, CacheOrigin::Cached);
        assert_eq!(two.body, one.body);
        assert_eq!(srv.hits(), 1, "cache hit must not touch the network");
    }

    #[test]
    fn authorization_change_produces_different_key_and_misses() {
        let srv = CounterServer::start(vec![(200, "{}"), (200, "{}")]);
        let opts = CacheOptions {
            dir: Some(temp_dir("auth-iso")),
            ttl_seconds: 3600,
        };
        let _ =
            get_json_cached(&opts, false, &srv.url, &[("Authorization", "Bearer alpha")]).unwrap();
        assert_eq!(srv.hits(), 1);
        let second =
            get_json_cached(&opts, false, &srv.url, &[("Authorization", "Bearer beta")]).unwrap();
        assert_eq!(
            second.origin,
            CacheOrigin::Live,
            "one credential must never receive another credential's cached body (FR-3.9)"
        );
        assert_eq!(srv.hits(), 2);
    }

    #[test]
    fn expired_entry_is_a_miss() {
        let srv = CounterServer::start(vec![(200, "{}"), (200, "{}")]);
        let dir = temp_dir("expired");
        let opts = CacheOptions {
            dir: Some(dir.clone()),
            ttl_seconds: 3600,
        };
        let _ = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        let key = cache_key_sha256("GET", &srv.url, &[]).unwrap();
        let env: CacheEnvelope =
            serde_json::from_slice(&fs::read(dir.join(&key)).unwrap()).unwrap();
        fs::write(
            dir.join(&key),
            serde_json::to_vec(&CacheEnvelope {
                observed_at_ms: now_ms().saturating_sub(3_601_000),
                ..env
            })
            .unwrap(),
        )
        .unwrap();
        let hit = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        assert_eq!(
            hit.origin,
            CacheOrigin::Live,
            "expired entries must miss (FR-3.6)"
        );
        assert_eq!(srv.hits(), 2);
    }

    #[test]
    fn future_dated_entry_is_a_miss() {
        let srv = CounterServer::start(vec![(200, "{}"), (200, "{}")]);
        let dir = temp_dir("future");
        let opts = CacheOptions {
            dir: Some(dir.clone()),
            ttl_seconds: 3600,
        };
        let _ = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        let key = cache_key_sha256("GET", &srv.url, &[]).unwrap();
        let env: CacheEnvelope =
            serde_json::from_slice(&fs::read(dir.join(&key)).unwrap()).unwrap();
        fs::write(
            dir.join(&key),
            serde_json::to_vec(&CacheEnvelope {
                observed_at_ms: now_ms() + 60_000,
                ..env
            })
            .unwrap(),
        )
        .unwrap();
        let hit = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        assert_eq!(
            hit.origin,
            CacheOrigin::Live,
            "future-dated entries must miss (FR-3.6)"
        );
        assert_eq!(srv.hits(), 2);
    }

    #[test]
    fn corrupt_entry_falls_back_to_live_request() {
        let srv = CounterServer::start(vec![(200, "{}"), (200, "{}")]);
        let dir = temp_dir("corrupt");
        let opts = CacheOptions {
            dir: Some(dir.clone()),
            ttl_seconds: 3600,
        };
        let _ = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        let key = cache_key_sha256("GET", &srv.url, &[]).unwrap();
        fs::write(dir.join(&key), b"not-json-at-all").unwrap();
        let hit = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        assert_eq!(
            hit.origin,
            CacheOrigin::Live,
            "corrupt entries fall back live (FR-3.8)"
        );
        assert_eq!(srv.hits(), 2);
    }

    #[test]
    fn fresh_bypasses_read_but_stores_for_later() {
        let srv = CounterServer::start(vec![(200, "{}"), (200, "{}")]);
        let opts = CacheOptions {
            dir: Some(temp_dir("fresh")),
            ttl_seconds: 3600,
        };
        let one = get_json_cached(&opts, true, &srv.url, &[]).unwrap();
        assert_eq!(one.origin, CacheOrigin::Live);
        let two = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        assert_eq!(
            two.origin,
            CacheOrigin::Cached,
            "fresh must store the live response (FR-3.2)"
        );
        assert_eq!(srv.hits(), 1);
        let three = get_json_cached(&opts, true, &srv.url, &[]).unwrap();
        assert_eq!(
            three.origin,
            CacheOrigin::Live,
            "fresh must bypass cached reads"
        );
        assert_eq!(srv.hits(), 2);
    }

    #[test]
    fn cache_files_never_contain_request_secrets() {
        let secret = "super-secret-bearer-value";
        let srv = CounterServer::start(vec![(200, r#"{"ok":true}"#)]);
        let dir = temp_dir("secrets");
        let opts = CacheOptions {
            dir: Some(dir.clone()),
            ttl_seconds: 3600,
        };
        let auth = format!("Bearer {secret}");
        let _ = get_json_cached(&opts, false, &srv.url, &[("Authorization", &auth)]).unwrap();
        let entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one entry file");
        let fname = entries[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(fname.len(), 64);
        assert!(fname.chars().all(|c| c.is_ascii_hexdigit()));
        let raw = fs::read_to_string(&entries[0]).unwrap();
        assert!(
            !raw.contains(secret),
            "envelope metadata must never contain request secrets (FR-3.5)"
        );
        let env: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let obj = env.as_object().expect("envelope must be an object");
        assert_eq!(
            obj.len(),
            3,
            "envelope holds exactly version/observed_at_ms/body"
        );
        assert!(obj.contains_key("version"));
        assert!(obj.contains_key("observed_at_ms"));
        assert!(obj.contains_key("body"));
    }

    #[test]
    fn cache_hit_carries_stored_observation_time() {
        let srv = CounterServer::start(vec![(200, "{}")]);
        let dir = temp_dir("provenance");
        let opts = CacheOptions {
            dir: Some(dir.clone()),
            ttl_seconds: 3600,
        };
        let one = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        assert_eq!(one.origin, CacheOrigin::Live);
        let key = cache_key_sha256("GET", &srv.url, &[]).unwrap();
        let path = dir.join(&key);
        let env: CacheEnvelope = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let backdated = now_ms().saturating_sub(5000);
        fs::write(
            &path,
            serde_json::to_vec(&CacheEnvelope {
                observed_at_ms: backdated,
                ..env
            })
            .unwrap(),
        )
        .unwrap();
        let two = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        assert_eq!(two.origin, CacheOrigin::Cached);
        assert_eq!(
            two.observed_at_ms, backdated,
            "a hit replays the original observation time, never now (FR-3.10)"
        );
        assert_eq!(srv.hits(), 1);
    }

    #[test]
    fn disabled_cache_is_pure_live_and_writes_nothing() {
        let srv = CounterServer::start(vec![(200, "{}"), (200, "{}")]);
        let dir = temp_dir("disabled");
        let opts = CacheOptions {
            dir: Some(dir.clone()),
            ttl_seconds: 0,
        };
        assert!(!opts.enabled());
        let one = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        let two = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        assert_eq!(one.origin, CacheOrigin::Live);
        assert_eq!(two.origin, CacheOrigin::Live);
        assert_eq!(
            srv.hits(),
            2,
            "zero TTL must never read or write the cache (FR-3.1)"
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_cache_dir_falls_back_live_with_note() {
        use std::os::unix::fs::PermissionsExt;
        let srv = CounterServer::start(vec![(200, "{}")]);
        let dir = temp_dir("denied");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let opts = CacheOptions {
            dir: Some(dir.clone()),
            ttl_seconds: 3600,
        };
        let hit = get_json_cached(&opts, false, &srv.url, &[]).unwrap();
        assert_eq!(
            hit.origin,
            CacheOrigin::Live,
            "cache failure never kills a live response (FR-3.8)"
        );
        assert_eq!(srv.hits(), 1);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    // -------------------------------------------------------------------
    // uncached POST helpers (AD-4)
    // -------------------------------------------------------------------

    #[test]
    fn post_json_parses_200_and_maps_status_errors() {
        let srv = CounterServer::start(vec![(200, r#"{"access_token":"t"}"#)]);
        let v = post_json(&srv.url, &[], &json!({"q": 1})).unwrap();
        assert_eq!(v["access_token"], "t");
        let raw = &srv.requests()[0];
        assert!(raw.starts_with("POST /v1/usage HTTP/1.1"));
        assert!(
            raw.to_ascii_lowercase()
                .contains("content-type: application/json"),
            "JSON POST must set the JSON content type"
        );

        let err_srv = CounterServer::start(vec![(500, r#"{"error":"boom"}"#)]);
        let e = post_json(&err_srv.url, &[], &json!({})).unwrap_err();
        let se = e
            .downcast_ref::<HttpStatusError>()
            .expect("non-2xx status must surface as HttpStatusError");
        assert_eq!(se.code, 500);
        assert!(
            se.note.contains("boom"),
            "note carries the one-line body snippet"
        );

        let nf = CounterServer::start(vec![(404, "nope")]);
        let e = post_json(&nf.url, &[], &json!({})).unwrap_err();
        assert_eq!(e.downcast_ref::<HttpStatusError>().unwrap().code, 404);
    }

    #[test]
    fn post_form_json_encodes_form_and_returns_json() {
        let srv = CounterServer::start(vec![(200, r#"{"ok":true}"#)]);
        let v = post_form_json(
            &srv.url,
            &[],
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", "abc/def=+x"),
                ("client_id", "9d1c250a"),
            ],
        )
        .unwrap();
        assert_eq!(v["ok"], true);
        let raw = &srv.requests()[0];
        assert!(raw.starts_with("POST /v1/usage HTTP/1.1"));
        assert!(
            raw.to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "form POST must set the form content type"
        );
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("client_id=9d1c250a"));
        assert!(
            !body.contains("abc/def"),
            "form values must be percent-encoded, got: {body}"
        );
    }

    // -------------------------------------------------------------------
    // debug output policy (NFR Security: no secrets in diagnostics)
    // -------------------------------------------------------------------

    #[test]
    fn post_debug_lines_never_expose_response_body_or_token() {
        let url = "https://oauth.example.test/token";
        let token = "fake-access-token-12345";
        let body = format!(r#"{{"access_token":"{token}","expires_in":3600}}"#);
        for status in ["200", "400"] {
            let line = debug_snippet("POST", url, status, &body);
            assert!(
                !line.contains(token),
                "POST debug must never expose the token: {line}"
            );
            assert!(
                !line.contains("expires_in"),
                "POST debug must never expose the response body: {line}"
            );
            assert!(
                line.contains("[debug] POST"),
                "method context must remain: {line}"
            );
            assert!(line.contains(url), "URL context must remain: {line}");
            assert!(line.contains(status), "status context must remain: {line}");
            assert!(
                line.contains("suppressed"),
                "the suppression must be explicit, not silent: {line}"
            );
        }
    }

    #[test]
    fn get_debug_lines_keep_payload_snippets() {
        let body = r#"{"quota":{"used":1,"limit":10}}"#;
        let line = debug_snippet("GET", "https://api.example.test/v1/quota", "200", body);
        assert!(
            line.contains(body),
            "GET debug keeps the payload-drift body snippet: {line}"
        );
        assert!(
            line.contains("[debug] GET"),
            "method context must remain: {line}"
        );
    }

    #[test]
    fn cost_break_note_404_is_none() {
        assert!(cost_break_note(&status_err(404), "anthropic").is_none());
    }

    #[test]
    fn cost_break_note_non_404_contains_provider_and_code() {
        let n = cost_break_note(&status_err(403), "anthropic").expect("403 must yield a note");
        assert!(n.contains("anthropic"));
        assert!(n.contains("403"));
        assert!(n.contains("billed totals may be incomplete"));
    }

    #[test]
    fn cost_break_note_transport_error_is_some() {
        let n = cost_break_note(&anyhow::anyhow!("connection refused"), "openai")
            .expect("transport error must yield a note");
        assert!(n.contains("openai"));
    }
}
