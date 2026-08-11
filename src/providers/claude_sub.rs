use super::{Provider, QuotaFetch};
use crate::{config::Config, credentials, http, types::*};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;

/// Pinned production OAuth client id (FR-6.3): the official Claude Code
/// `2.1.227` Linux x64 release asset's embedded refresh function. Public
/// upstream installed-app identifier, not a user secret.
const PROD_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Proactive refresh horizon (FR-6.2): refresh when
/// `expiresAt <= now + 300000ms`.
const PROACTIVE_LEAD_MS: i64 = 300_000;

/// Refresh-token expiry warning horizon (FR-6.9): three days.
const REFRESH_WARNING_MS: i64 = 3 * 24 * 60 * 60 * 1000;

/// Claude Pro/Max subscription — live Session / Weekly / Opus meters.
///
/// Claude Code's own `/usage` screen calls
///   GET https://api.anthropic.com/api/oauth/usage
/// with the OAuth access token it stores on login. The endpoint is not
/// in the public API reference, but multiple trackers ship it
/// (robinebers/openusage, steipete/CodexBar). Required headers:
///   Authorization: Bearer <accessToken>
///   anthropic-beta: oauth-2025-04-20
///
/// Token discovery: `claudeAiOauth` inside `.credentials.json` in
/// $CLAUDE_CONFIG_DIR / ~/.claude / ~/.config/claude (on macOS, Claude
/// Code may keep it in the keychain instead — point [claude].credentials
/// at an exported copy then).
///
/// Response shape: `five_hour` / `seven_day` / `seven_day_sonnet`
/// objects with {utilization, resets_at}, plus a `limits[]` array of
/// model-scoped weekly meters (kind == "weekly_scoped").
///
/// OAuth refresh (FR-6): file-backed `claudeAiOauth` entries with a
/// refresh token and millisecond `expiresAt` are refreshed proactively
/// when `expiresAt <= now + 300000ms` through the shared locked
/// credential transaction (FR-4.3: the llmu lock is held across the
/// token POST). A usage 401 re-reads the file, adopts a token another
/// process installed, or forces exactly one refresh and retries once.
/// Direct access tokens (e.g. OpenCode's) are never refreshed.
///
/// The endpoint rate-limits aggressively; llmu calls it once per run.
pub struct ClaudeSub;

fn window(v: &serde_json::Value, plan: &str, window: &str) -> Option<QuotaSnapshot> {
    let used = v["utilization"].as_f64()?;
    Some(QuotaSnapshot {
        provider: "claude".into(),
        plan: plan.into(),
        window: window.into(),
        used,
        limit: 100.0,
        unit: "%".into(),
        resets_at: v["resets_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc)),
    })
}

/// Existing quota payload normalization (FR-6.10): session meters plus
/// model-scoped weekly meters under `limits[]`, sorted by payload order.
fn parse_usage(v: &serde_json::Value, plan: &str, out: &mut Vec<QuotaSnapshot>) {
    if let Some(q) = window(&v["five_hour"], plan, "5h") {
        out.push(q);
    }
    if let Some(q) = window(&v["seven_day"], plan, "7d") {
        out.push(q);
    }
    if let Some(q) = window(&v["seven_day_sonnet"], plan, "7d-sonnet") {
        out.push(q);
    }
    // Newer payloads: model-scoped weekly meters under `limits[]`.
    for l in v["limits"].as_array().unwrap_or(&vec![]) {
        if l["kind"].as_str() == Some("weekly_scoped") {
            let label = l["model"]
                .as_str()
                .map(|m| format!("7d-{m}"))
                .unwrap_or_else(|| "7d-scoped".into());
            if let Some(q) = window(l, plan, &label) {
                out.push(q);
            }
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The provider-owned slice of `claudeAiOauth` that refresh needs
/// (FR-6.1). Everything else in the root object — unknown fields
/// included — is preserved by the shared transaction's merge closure.
struct ClaudeOauth {
    token: String,
    expires_at: Option<i64>,
    refresh_token: Option<String>,
    refresh_token_expires_at: Option<i64>,
    scopes: Vec<String>,
    client_id: Option<String>,
    sub_type: Option<String>,
}

impl ClaudeOauth {
    fn from_root(root: &serde_json::Value) -> Result<Self> {
        let o = root
            .get("claudeAiOauth")
            .context("no claudeAiOauth in credentials file")?;
        let token = o["accessToken"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("no claudeAiOauth.accessToken in credentials file")?
            .to_string();
        // FR-6.1: subscription metadata is read (and preserved verbatim
        // through refresh by unknown-field preservation).
        let _rate_limit_tier = o["rateLimitTier"].as_str();
        Ok(Self {
            token,
            expires_at: o["expiresAt"].as_i64(),
            refresh_token: o["refreshToken"]
                .as_str()
                .map(String::from)
                .filter(|s| !s.is_empty()),
            refresh_token_expires_at: o["refreshTokenExpiresAt"].as_i64(),
            scopes: match o["scopes"].as_array() {
                Some(a) => a
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                None => o["scopes"]
                    .as_str()
                    .map(|s| s.split(' ').map(String::from).collect())
                    .unwrap_or_default(),
            },
            client_id: o["clientId"]
                .as_str()
                .map(String::from)
                .filter(|s| !s.is_empty()),
            sub_type: o["subscriptionType"].as_str().map(String::from),
        })
    }

    /// The access token has not yet expired.
    fn unexpired(&self) -> bool {
        self.expires_at.map(|e| e > now_ms()).unwrap_or(false)
    }
}

/// FR-6.2: proactive refresh when `expiresAt <= now + 300000ms`. Missing
/// expiry metadata falls back to the old read-only behavior (use the
/// token as-is; a later 401 still drives the reactive path).
fn needs_refresh(s: &ClaudeOauth) -> bool {
    s.expires_at
        .map(|e| e <= now_ms() + PROACTIVE_LEAD_MS)
        .unwrap_or(false)
}

/// The refresh POST payload (FR-6.3): grant type, stored refresh token,
/// stored `clientId` or the pinned production id, and space-joined
/// scopes. Never a client secret, never a bearer header.
fn refresh_payload(s: &ClaudeOauth, refresh_token: &str) -> serde_json::Value {
    let mut p = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": s.client_id.clone().unwrap_or_else(|| PROD_CLIENT_ID.to_string()),
    });
    if !s.scopes.is_empty() {
        p["scope"] = serde_json::json!(s.scopes.join(" "));
    }
    p
}

/// A refresh failure that a login will cure (invalid_grant and friends):
/// 4xx from the token endpoint, except timeout/rate-limit codes that are
/// transient. Transport and 5xx failures are transient.
fn refresh_failure_is_permanent(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<http::HttpStatusError>() {
        Some(se) => (400..500).contains(&se.code) && se.code != 408 && se.code != 429,
        None => false,
    }
}

/// The token-endpoint CAS schema for `claudeAiOauth` (provider-owned,
/// AD-2). The shared transaction derives the expected refresh token from
/// the locked snapshot, so a stale request token can never be written.
fn claude_refresh_token(v: &serde_json::Value) -> Option<&str> {
    v["claudeAiOauth"]["refreshToken"].as_str()
}

fn claude_validate(v: &serde_json::Value) -> Result<()> {
    let o = &v["claudeAiOauth"];
    o["accessToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("claudeAiOauth.accessToken missing after refresh")?;
    o["expiresAt"]
        .as_i64()
        .filter(|e| *e > 0)
        .context("claudeAiOauth.expiresAt missing after refresh")?;
    o["refreshToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("claudeAiOauth.refreshToken missing after refresh")?;
    Ok(())
}

fn claude_schema() -> credentials::CredentialSchema<'static> {
    credentials::CredentialSchema {
        refresh_token: &claude_refresh_token,
        validate: &claude_validate,
    }
}

/// Internal endpoint seam (AD-4): production uses the fixed endpoints;
/// tests inject a local `TcpListener` URL so no fixture ever hits the
/// real APIs or the user's credential stores.
#[derive(Debug, Clone)]
struct Endpoints {
    token: String,
    usage: String,
}

impl Endpoints {
    fn prod() -> Self {
        Self {
            token: "https://platform.claude.com/v1/oauth/token".into(),
            usage: "https://api.anthropic.com/api/oauth/usage".into(),
        }
    }
}

/// FR-6.2/6.6: the locked refresh transaction. `rejected` carries the
/// token that just failed usage with 401: adoption then accepts only a
/// token another process installed meanwhile, and a refresh is forced
/// even when the old token is not expiring. With `rejected == None`
/// (proactive path) a usable token under the lock is adopted without
/// HTTP (FR-6.2: re-read and adopt a token another process already
/// refreshed).
fn refresh_or_adopt(
    path: &Path,
    ep: &Endpoints,
    notes: &mut Vec<String>,
    rejected: Option<&str>,
) -> Result<String> {
    // FR-4.3: never send the refresh request before the llmu lock is
    // held; the guard lives across the token POST and the persistence.
    let locked = credentials::lock_and_read(&credentials::LockTiming::default(), path)?;
    let state = ClaudeOauth::from_root(locked.value())?;
    let adopt = match rejected {
        Some(bad) => state.token != bad && state.unexpired(),
        None => !needs_refresh(&state),
    };
    if adopt {
        drop(locked);
        return Ok(state.token);
    }
    let refresh_token = state.refresh_token.as_deref().context(
        "claudeAiOauth has no refreshToken and the access token is expiring or rejected — log in again with Claude Code to restore quota monitoring",
    )?;

    // Hold `LockedCredential` across the OAuth HTTP request (FR-4.3).
    let resp = match http::post_json(
        &ep.token,
        &[("User-Agent", "llmu"), ("Accept", "application/json")],
        &refresh_payload(&state, refresh_token),
    ) {
        Ok(v) => v,
        Err(e) => {
            if refresh_failure_is_permanent(&e) {
                // FR-6.7: permanent OAuth failures preserve the credential
                // file (the guard drops without a write) and instruct
                // login. Nothing is retried.
                return Err(anyhow::anyhow!(
                    "token refresh was rejected — log in again with Claude Code ({e})"
                ));
            }
            if rejected.is_none() && state.unexpired() {
                // FR-6.5: a transient proactive failure falls back to the
                // still-valid access token with a warning.
                notes.push(format!(
                    "claude: token refresh failed ({e}) — continuing with the current access token until it expires"
                ));
                drop(locked);
                return Ok(state.token);
            }
            return Err(e).context("token refresh failed");
        }
    };

    // FR-6.4: validate every required response field before persistence.
    let access = resp["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("oauth token response missing access_token")?;
    let expires_in = resp["expires_in"]
        .as_i64()
        .filter(|v| *v > 0)
        .context("oauth token response missing positive expires_in")?;
    if resp["scope"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        bail!("oauth token response missing usable scope");
    }
    // Effective refresh token: a rotated one, else the request token
    // (guaranteed nonempty above).
    let rotated = resp["refresh_token"]
        .as_str()
        .map(String::from)
        .filter(|s| !s.is_empty());
    let rel_expiry = resp["refresh_token_expires_in"].as_i64().filter(|v| *v > 0);
    let now = now_ms();

    // Consuming `replace_with_cas` persists through the SAME guard:
    // rotate the access token and expiry, a returned refresh token, and
    // a returned relative refresh-token expiry — every unknown field and
    // `refreshTokenExpiresAt` (when its relative field is absent) survive
    // by construction (FR-6.4).
    let outcome = locked.replace_with_cas(&claude_schema(), |v| {
        let mut m = v.clone();
        let o = &mut m["claudeAiOauth"];
        o["accessToken"] = serde_json::json!(access);
        o["expiresAt"] = serde_json::json!(now + expires_in * 1000);
        if let Some(r) = &rotated {
            o["refreshToken"] = serde_json::json!(r);
        }
        if let Some(rel) = rel_expiry {
            o["refreshTokenExpiresAt"] = serde_json::json!(now + rel * 1000);
        }
        m
    })?;
    let token = match outcome {
        credentials::ReplaceOutcome::Replaced { value }
        | credentials::ReplaceOutcome::ChangedByOther { value } => value["claudeAiOauth"]
            ["accessToken"]
            .as_str()
            .context("refreshed credential lost accessToken")?
            .to_string(),
    };
    Ok(token)
}

fn usage_get(token: &str, ep: &Endpoints) -> Result<serde_json::Value> {
    let auth = format!("Bearer {token}");
    http::get_json(
        &ep.usage,
        &[
            ("Authorization", auth.as_str()),
            ("Accept", "application/json"),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("User-Agent", "llmu"),
        ],
    )
}

fn usage_is_401(e: &anyhow::Error) -> bool {
    e.downcast_ref::<http::HttpStatusError>()
        .map(|se| se.code == 401)
        .unwrap_or(false)
}

/// Existing 429 messaging survives verbatim (FR-6.10); every other error
/// passes through unchanged.
fn map_usage_error(e: anyhow::Error) -> anyhow::Error {
    if e.to_string().contains("HTTP 429") {
        anyhow::anyhow!(
            "oauth/usage rate-limited (this endpoint throttles hard) — retry in a few minutes"
        )
    } else {
        e
    }
}

/// File-backed `claudeAiOauth` flow (FR-6.1 through FR-6.7, FR-6.9).
fn quota_file_backed(path: &Path, ep: &Endpoints) -> Result<QuotaFetch> {
    let root = credentials::read_json(path)?;
    let state = ClaudeOauth::from_root(&root)?;
    let mut notes = vec![];
    // FR-6.9: warn when the refresh token expires within three days,
    // without blocking an otherwise valid quota request.
    if let Some(exp) = state.refresh_token_expires_at {
        if exp <= now_ms() + REFRESH_WARNING_MS {
            notes.push(
                "claude: refresh token expires within three days — re-authenticate with your Anthropic client soon".into(),
            );
        }
    }

    let token = if needs_refresh(&state) {
        refresh_or_adopt(path, ep, &mut notes, None)?
    } else {
        state.token.clone()
    };

    let usage = match usage_get(&token, ep) {
        Ok(v) => v,
        Err(e) if usage_is_401(&e) => {
            // FR-6.6: exactly one reactive retry for file-backed
            // credentials: re-read, adopt a changed usable token, else
            // force one refresh, then retry once. Never refresh/retry on
            // 403, 429, or 5xx (those fall through `map_usage_error`).
            let root2 = credentials::read_json(path)?;
            let s2 = ClaudeOauth::from_root(&root2)?;
            let retry = if s2.token != token && s2.unexpired() {
                s2.token
            } else {
                refresh_or_adopt(path, ep, &mut notes, Some(&token))?
            };
            usage_get(&retry, ep).map_err(|e| {
                if usage_is_401(&e) {
                    anyhow::anyhow!(
                        "oauth/usage 401 persists after one refresh — log in again with Claude Code"
                    )
                } else {
                    map_usage_error(e)
                }
            })?
        }
        Err(e) => return Err(map_usage_error(e)),
    };

    let plan = state
        .sub_type
        .unwrap_or_else(|| "Claude subscription".into());
    let mut out = vec![];
    parse_usage(&usage, &plan, &mut out);
    Ok(QuotaFetch {
        snapshots: out,
        notes,
        refresh_last_known_good: true,
    })
}

/// Direct access-token flow (e.g. OpenCode's auth.json): read-only, never
/// refreshed (FR-4.1, FR-6.8).
fn quota_direct(cfg: &Config, ep: &Endpoints) -> Result<QuotaFetch> {
    let Some(token) = &cfg.claude.access_token else {
        return Ok(QuotaFetch::default());
    };
    let usage = usage_get(token, ep).map_err(|e| {
        if usage_is_401(&e) {
            // FR-6.8: source-specific remediation — name OpenCode, never
            // claim llmu can refresh an access-only token.
            anyhow::anyhow!(
                "oauth/usage 401: expired access token — refresh Anthropic authentication in OpenCode or configure Claude Code credentials; llmu cannot refresh direct access tokens"
            )
        } else {
            map_usage_error(e)
        }
    })?;
    let mut out = vec![];
    parse_usage(&usage, "Claude subscription", &mut out);
    Ok(QuotaFetch::live(out))
}

/// Internal entry point with an injectable endpoint bundle (AD-4).
fn quotas_impl(cfg: &Config, ep: &Endpoints) -> Result<QuotaFetch> {
    match cfg.claude.credentials_path() {
        Some(path) => quota_file_backed(&path, ep),
        None => quota_direct(cfg, ep),
    }
}

impl Provider for ClaudeSub {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn configured(&self, cfg: &Config) -> bool {
        cfg.claude.credentials_path().is_some() || cfg.claude.access_token.is_some()
    }
    fn capabilities(&self) -> &'static str {
        "Pro/Max live session + weekly quotas via api.anthropic.com/api/oauth/usage (Claude Code OAuth token, auto-refreshed)"
    }

    fn quotas(&self, cfg: &Config) -> Result<QuotaFetch> {
        quotas_impl(cfg, &Endpoints::prod())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials;
    use serde_json::{json, Value};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::SystemTime;

    static TEST_DIR_NONCE: AtomicU64 = AtomicU64::new(0);

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = TEST_DIR_NONCE.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "llmu-claude-t5-{}-{}-{nonce}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn cfg_for(path: PathBuf) -> Config {
        let mut cfg = Config::default();
        cfg.claude.credentials = Some(path);
        cfg.claude.access_token = None;
        cfg
    }

    /// Write a Claude Code credentials file: `claudeAiOauth` plus unknown
    /// root and nested fields that must survive refresh (FR-6.1).
    fn write_oauth(dir: &Path, oauth: &Value) -> PathBuf {
        let p = dir.join(".credentials.json");
        let root = json!({
            "claudeAiOauth": oauth,
            "unknown_root": {"keep": [1, 2]}
        });
        fs::write(&p, serde_json::to_vec_pretty(&root).unwrap()).unwrap();
        p
    }

    fn oauth(access: &str, refresh: Option<&str>, expires_at: i64) -> Value {
        let mut o = json!({
            "accessToken": access,
            "expiresAt": expires_at,
            "scopes": ["openid", "offline_access"],
            "subscriptionType": "Pro",
            "rateLimitTier": "pro",
            "clientId": "stored-client",
            "meta": {"unknown_inner": "keep"}
        });
        if let Some(r) = refresh {
            o["refreshToken"] = json!(r);
        }
        o
    }

    /// Access token expiring within the five-minute proactive horizon but
    /// still usable.
    fn oauth_expiring(access: &str, refresh: Option<&str>) -> Value {
        oauth(access, refresh, now_ms() + 60_000)
    }

    /// Access token far outside the proactive horizon.
    fn oauth_valid(access: &str, refresh: Option<&str>) -> Value {
        oauth(access, refresh, now_ms() + 86_400_000)
    }

    const TOKEN_RESPONSE: &str = r#"{"access_token":"AT2","expires_in":3600,"refresh_token":"R2","scope":"openid offline_access","token_type":"Bearer"}"#;
    const TOKEN_NO_ROTATION: &str = r#"{"access_token":"AT2","expires_in":3600,"scope":"openid offline_access","token_type":"Bearer"}"#;
    const TOKEN_ROTATED_REL: &str = r#"{"access_token":"AT2","expires_in":3600,"refresh_token":"R2","scope":"openid offline_access","refresh_token_expires_in":1209600}"#;
    const USAGE_RESPONSE: &str = r#"{"five_hour":{"utilization":12.5,"resets_at":"2026-08-11T12:00:00Z"},"seven_day":{"utilization":45.0,"resets_at":"2026-08-12T12:00:00Z"},"limits":[{"kind":"weekly_scoped","model":"opus","utilization":60.0,"resets_at":"2026-08-12T12:00:00Z"}]}"#;

    #[derive(Clone)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: String,
        body: String,
    }

    /// Local HTTP fixture for both the token POST and the usage GET, with
    /// per-response hooks that run after the request is read and before
    /// the response is written (deterministic mid-flight file mutation).
    struct FixtureServer {
        endpoints: Endpoints,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    impl FixtureServer {
        fn start(responses: Vec<ScriptedResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let requests: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(vec![]));
            let r2 = requests.clone();
            thread::spawn(move || {
                for mut r in responses {
                    let (mut sock, _) = listener.accept().unwrap();
                    let buf = read_request(&mut sock);
                    let req = parse_request(&buf);
                    r2.lock().unwrap().push(req);
                    if let Some(h) = r.hook.take() {
                        h();
                    }
                    let reason = match r.status {
                        200 => "OK",
                        400 => "Bad Request",
                        401 => "Unauthorized",
                        403 => "Forbidden",
                        429 => "Too Many Requests",
                        500 => "Internal Server Error",
                        _ => "X",
                    };
                    let resp = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        r.status,
                        reason,
                        r.body.len(),
                        r.body
                    );
                    sock.write_all(resp.as_bytes()).unwrap();
                }
            });
            let port = addr.port();
            Self {
                endpoints: Endpoints {
                    token: format!("http://127.0.0.1:{port}/v1/oauth/token"),
                    usage: format!("http://127.0.0.1:{port}/v1/usage"),
                },
                requests,
            }
        }

        fn requests(&self) -> Vec<CapturedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    struct ScriptedResponse {
        method: &'static str,
        path: &'static str,
        status: u16,
        body: &'static str,
        hook: Option<Box<dyn FnOnce() + Send>>,
    }

    impl ScriptedResponse {
        fn token(status: u16, body: &'static str) -> Self {
            Self {
                method: "POST",
                path: "/v1/oauth/token",
                status,
                body,
                hook: None,
            }
        }
        fn usage(status: u16, body: &'static str) -> Self {
            Self {
                method: "GET",
                path: "/v1/usage",
                status,
                body,
                hook: None,
            }
        }
        fn token_ok() -> Self {
            Self::token(200, TOKEN_RESPONSE)
        }
        fn usage_ok() -> Self {
            Self::usage(200, USAGE_RESPONSE)
        }
    }

    fn read_request(sock: &mut TcpStream) -> Vec<u8> {
        let mut buf: Vec<u8> = vec![];
        let mut tmp = [0u8; 4096];
        loop {
            let n = sock.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf);
                let cl = head.lines().find_map(|l| {
                    l.strip_prefix("Content-Length:")
                        .or_else(|| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                });
                if let Some(cl) = cl {
                    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
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
        buf
    }

    fn parse_request(buf: &[u8]) -> CapturedRequest {
        let s = String::from_utf8_lossy(buf).into_owned();
        let (head, body) = match s.split_once("\r\n\r\n") {
            Some((h, b)) => (h.to_string(), b.to_string()),
            None => (s.clone(), String::new()),
        };
        let mut parts = head.lines().next().unwrap_or("").split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        CapturedRequest {
            method,
            path,
            headers: head.to_ascii_lowercase(),
            body,
        }
    }

    fn on_disk(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn proactive_refresh_within_five_minutes_posts_and_persists_rotated_token() {
        let dir = temp_dir("proactive");
        let path = write_oauth(&dir, &oauth_expiring("AT1", Some("R1")));
        let srv = FixtureServer::start(vec![
            ScriptedResponse::token_ok(),
            ScriptedResponse::usage_ok(),
        ]);
        let f = quotas_impl(&cfg_for(path.clone()), &srv.endpoints).unwrap();
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 2, "exactly one refresh POST then one usage GET");
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/v1/oauth/token");
        assert_eq!(reqs[1].method, "GET");
        assert_eq!(reqs[1].path, "/v1/usage");
        let o = &on_disk(&path)["claudeAiOauth"];
        assert_eq!(o["accessToken"], "AT2", "the rotated access token persists");
        assert_eq!(
            o["refreshToken"], "R2",
            "the rotated refresh token persists"
        );
        assert!(
            o["expiresAt"].as_i64().unwrap() > now_ms() + 3_500_000,
            "expiresAt must be recomputed from the positive expires_in"
        );
        assert_eq!(o["subscriptionType"], "Pro");
        assert_eq!(o["rateLimitTier"], "pro");
        assert_eq!(o["scopes"], json!(["openid", "offline_access"]));
        assert_eq!(
            o["meta"]["unknown_inner"], "keep",
            "unknown nested fields survive (FR-4.2)"
        );
        assert_eq!(
            on_disk(&path)["unknown_root"]["keep"],
            json!([1, 2]),
            "unknown root fields survive (FR-4.2)"
        );
        assert_eq!(f.snapshots.len(), 3, "5h + 7d + weekly scoped rows");
        assert!(
            f.snapshots.iter().all(|q| q.plan == "Pro"),
            "plan comes from subscriptionType"
        );
        assert!(
            f.refresh_last_known_good,
            "live rows mark last-known-good refresh (AD-4)"
        );
        assert!(f.notes.is_empty());
    }

    #[test]
    fn valid_token_outside_refresh_horizon_skips_refresh_entirely() {
        let dir = temp_dir("no-proactive");
        let path = write_oauth(&dir, &oauth_valid("AT1", Some("R1")));
        let before = fs::read(&path).unwrap();
        let srv = FixtureServer::start(vec![ScriptedResponse::usage_ok()]);
        let f = quotas_impl(&cfg_for(path.clone()), &srv.endpoints).unwrap();
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "no refresh means no write"
        );
        assert_eq!(f.snapshots.len(), 3);
    }

    #[test]
    fn refresh_payload_is_exact_grant_client_and_joined_scopes() {
        let dir = temp_dir("payload");
        let mut o = oauth_expiring("AT1", Some("R1"));
        o["scopes"] = json!(["openid", "offline_access", "anthropic-ai-studio:read"]);
        let path = write_oauth(&dir, &o);
        let srv = FixtureServer::start(vec![
            ScriptedResponse::token_ok(),
            ScriptedResponse::usage_ok(),
        ]);
        quotas_impl(&cfg_for(path), &srv.endpoints).unwrap();
        let reqs = srv.requests();
        let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
        let obj = body
            .as_object()
            .expect("token POST body must be a JSON object");
        assert_eq!(
            obj.len(),
            4,
            "grant_type + refresh_token + client_id + scope only"
        );
        assert_eq!(obj["grant_type"], "refresh_token");
        assert_eq!(obj["refresh_token"], "R1");
        assert_eq!(
            obj["client_id"], "stored-client",
            "the stored clientId wins"
        );
        assert_eq!(
            obj["scope"],
            "openid offline_access anthropic-ai-studio:read"
        );
        // The literal name is assembled so the contract test's
        // whole-file secret scan stays meaningful (FR-6.3).
        let forbidden_key = format!("client_{}", "secret");
        assert!(
            !obj.contains_key(&forbidden_key),
            "no client secret in the refresh payload (FR-6.3)"
        );
        assert!(
            !reqs[0].headers.contains("authorization"),
            "no bearer header on the token POST (FR-6.3)"
        );
    }

    #[test]
    fn refresh_payload_falls_back_to_pinned_production_client_id() {
        let dir = temp_dir("prod-client");
        let mut o = oauth_expiring("AT1", Some("R1"));
        o.as_object_mut().unwrap().remove("clientId");
        let path = write_oauth(&dir, &o);
        let srv = FixtureServer::start(vec![
            ScriptedResponse::token_ok(),
            ScriptedResponse::usage_ok(),
        ]);
        quotas_impl(&cfg_for(path), &srv.endpoints).unwrap();
        let body: Value = serde_json::from_str(&srv.requests()[0].body).unwrap();
        assert_eq!(body["client_id"], PROD_CLIENT_ID);
    }

    #[test]
    fn rotated_token_and_relative_expiry_persist_when_present() {
        let dir = temp_dir("rotated");
        let mut o = oauth_expiring("AT1", Some("R1"));
        o["refreshTokenExpiresAt"] = json!(now_ms() + 1_000_000);
        let path = write_oauth(&dir, &o);
        let srv = FixtureServer::start(vec![
            ScriptedResponse::token(200, TOKEN_ROTATED_REL),
            ScriptedResponse::usage_ok(),
        ]);
        quotas_impl(&cfg_for(path.clone()), &srv.endpoints).unwrap();
        let o = &on_disk(&path)["claudeAiOauth"];
        assert_eq!(o["refreshToken"], "R2", "a rotated refresh token persists");
        assert!(
            o["refreshTokenExpiresAt"].as_i64().unwrap() > now_ms() + 1_200_000_000,
            "the relative refresh_token_expires_in field advances refreshTokenExpiresAt"
        );
    }

    #[test]
    fn response_without_rotation_retains_request_token_and_refresh_expiry() {
        let dir = temp_dir("retain");
        let mut o = oauth_expiring("AT1", Some("R1"));
        let old_rel = now_ms() + 500_000;
        o["refreshTokenExpiresAt"] = json!(old_rel);
        let path = write_oauth(&dir, &o);
        let srv = FixtureServer::start(vec![
            ScriptedResponse::token(200, TOKEN_NO_ROTATION),
            ScriptedResponse::usage_ok(),
        ]);
        quotas_impl(&cfg_for(path.clone()), &srv.endpoints).unwrap();
        let o = &on_disk(&path)["claudeAiOauth"];
        assert_eq!(
            o["refreshToken"], "R1",
            "the request token is retained when the response omits rotation"
        );
        assert_eq!(
            o["refreshTokenExpiresAt"].as_i64().unwrap(),
            old_rel,
            "refreshTokenExpiresAt is preserved when its relative field is absent (FR-6.4)"
        );
    }

    #[test]
    fn adopts_token_another_process_installed_under_lock_without_http() {
        let dir = temp_dir("adopt-lock");
        let path = write_oauth(&dir, &oauth_expiring("AT1", Some("R1")));
        let srv = FixtureServer::start(vec![ScriptedResponse::usage_ok()]);
        let (held_tx, held_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let path2 = path.clone();
        let child = thread::spawn(move || {
            // Another llmu process: holds the llmu lock, installs a fresh
            // token, then releases.
            let locked =
                credentials::lock_and_read(&credentials::LockTiming::default(), &path2).unwrap();
            held_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            let mut root = locked.value().clone();
            root["claudeAiOauth"]["accessToken"] = json!("AT2");
            root["claudeAiOauth"]["expiresAt"] = json!(now_ms() + 86_400_000);
            // Atomic replacement, like a real upstream client: readers
            // see either the old or the new content, never a truncation.
            let tmp = path2.with_file_name(".adopt.tmp");
            fs::write(&tmp, serde_json::to_vec_pretty(&root).unwrap()).unwrap();
            fs::rename(&tmp, &path2).unwrap();
            drop(locked);
        });
        held_rx.recv().unwrap();
        let path3 = path.clone();
        let ep = srv.endpoints.clone();
        let handle = thread::spawn(move || quotas_impl(&cfg_for(path3), &ep).unwrap());
        go_tx.send(()).unwrap();
        let f = handle.join().unwrap();
        child.join().unwrap();
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1, "adoption must not hit the token endpoint");
        assert_eq!(reqs[0].method, "GET");
        assert!(
            reqs[0].headers.contains("authorization: bearer at2"),
            "the usage request must use the adopted token"
        );
        assert_eq!(f.snapshots.len(), 3);
    }

    #[test]
    fn transient_proactive_failure_falls_back_to_valid_token_with_warning() {
        let dir = temp_dir("transient");
        let path = write_oauth(&dir, &oauth_expiring("AT1", Some("R1")));
        let before = fs::read(&path).unwrap();
        let srv = FixtureServer::start(vec![
            ScriptedResponse::token(500, r#"{"error":"boom"}"#),
            ScriptedResponse::usage_ok(),
        ]);
        let f = quotas_impl(&cfg_for(path.clone()), &srv.endpoints).unwrap();
        assert_eq!(
            f.snapshots.len(),
            3,
            "fallback must not block quota collection"
        );
        assert_eq!(
            f.notes.len(),
            1,
            "a transient proactive failure surfaces exactly one warning (FR-6.5)"
        );
        assert!(f.notes[0].contains("token refresh failed"));
        assert!(f.notes[0].contains("continuing"));
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "failed refresh must leave the credential file unchanged"
        );
    }

    #[test]
    fn permanent_token_rejection_preserves_credentials_and_instructs_login() {
        let dir = temp_dir("permanent");
        let path = write_oauth(&dir, &oauth_expiring("AT1", Some("R1")));
        let before = fs::read(&path).unwrap();
        let srv = FixtureServer::start(vec![ScriptedResponse::token(
            400,
            r#"{"error":"invalid_grant"}"#,
        )]);
        let err = quotas_impl(&cfg_for(path.clone()), &srv.endpoints)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("log in"),
            "permanent rejection instructs login (FR-6.7): {err}"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "credentials are preserved byte-identical"
        );
        let reqs = srv.requests();
        assert_eq!(
            reqs.len(),
            1,
            "no usage request after a permanent rejection"
        );
        assert_eq!(reqs[0].method, "POST");
        assert!(
            fs::read_dir(&dir).unwrap().count() == 1,
            "no temp or lock files may survive"
        );
    }

    #[test]
    fn usage_401_forces_one_refresh_and_retries_exactly_once() {
        let dir = temp_dir("retry-once");
        let path = write_oauth(&dir, &oauth_valid("AT1", Some("R1")));
        let srv = FixtureServer::start(vec![
            ScriptedResponse::usage(401, r#"{"error":"unauthorized"}"#),
            ScriptedResponse::token_ok(),
            ScriptedResponse::usage_ok(),
        ]);
        let f = quotas_impl(&cfg_for(path.clone()), &srv.endpoints).unwrap();
        let reqs = srv.requests();
        assert_eq!(
            reqs.len(),
            3,
            "GET 401 -> one refresh POST -> one retry GET"
        );
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[1].method, "POST");
        assert_eq!(reqs[2].method, "GET");
        assert!(
            reqs[2].headers.contains("authorization: bearer at2"),
            "the retry must use the refreshed token"
        );
        assert_eq!(on_disk(&path)["claudeAiOauth"]["accessToken"], "AT2");
        assert_eq!(f.snapshots.len(), 3);
    }

    #[test]
    fn usage_401_adopts_changed_usable_token_from_disk_without_refresh() {
        let dir = temp_dir("adopt-401");
        let path = write_oauth(&dir, &oauth_valid("AT1", Some("R1")));
        let path2 = path.clone();
        let hook = Box::new(move || {
            // Another process (upstream client) landed a fresh token
            // between our read and the 401 response.
            let root = json!({
                "claudeAiOauth": oauth_valid("AT2", Some("R1")),
                "unknown_root": {"keep": [1, 2]}
            });
            fs::write(&path2, serde_json::to_vec_pretty(&root).unwrap()).unwrap();
        });
        let srv = FixtureServer::start(vec![
            ScriptedResponse {
                method: "GET",
                path: "/v1/usage",
                status: 401,
                body: r#"{"error":"unauthorized"}"#,
                hook: Some(hook),
            },
            ScriptedResponse::usage_ok(),
        ]);
        let f = quotas_impl(&cfg_for(path), &srv.endpoints).unwrap();
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 2, "adoption must not touch the token endpoint");
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[1].method, "GET");
        assert!(
            reqs[1].headers.contains("authorization: bearer at2"),
            "the retry uses the adopted token"
        );
        assert_eq!(f.snapshots.len(), 3);
    }

    #[test]
    fn second_usage_401_fails_without_further_refresh() {
        let dir = temp_dir("double-401");
        let path = write_oauth(&dir, &oauth_valid("AT1", Some("R1")));
        let srv = FixtureServer::start(vec![
            ScriptedResponse::usage(401, r#"{"error":"unauthorized"}"#),
            ScriptedResponse::token_ok(),
            ScriptedResponse::usage(401, r#"{"error":"unauthorized"}"#),
        ]);
        let err = quotas_impl(&cfg_for(path), &srv.endpoints)
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "the retried 401 surfaces: {err}");
        assert!(err.contains("log in"));
        let reqs = srv.requests();
        assert_eq!(
            reqs.len(),
            3,
            "exactly one refresh and one retry — never loops (AS-4)"
        );
    }

    #[test]
    fn no_refresh_or_retry_on_usage_403_429_or_5xx() {
        for (status, body) in [
            (403u16, "forbidden"),
            (429u16, "rate limited"),
            (500u16, "boom"),
        ] {
            let dir = temp_dir(&format!("status-{status}"));
            let path = write_oauth(&dir, &oauth_valid("AT1", Some("R1")));
            let srv = FixtureServer::start(vec![ScriptedResponse::usage(status, body)]);
            let err = quotas_impl(&cfg_for(path), &srv.endpoints).unwrap_err();
            let msg = err.to_string();
            assert!(
                !msg.contains("token refresh"),
                "a {status} usage response must not trigger refresh: {msg}"
            );
            if status == 429 {
                assert_eq!(
                    msg,
                    "oauth/usage rate-limited (this endpoint throttles hard) — retry in a few minutes",
                    "the exact existing 429 message survives (FR-6.10)"
                );
            }
            let reqs = srv.requests();
            assert_eq!(reqs.len(), 1, "no refresh and no retry on {status}");
            assert_eq!(reqs[0].method, "GET");
        }
    }

    #[test]
    fn direct_access_401_names_opencode_and_never_claims_llmu_can_refresh() {
        let mut cfg = Config::default();
        cfg.claude.credentials = Some("/nonexistent/llmu-t5-opencode-test".into());
        cfg.claude.access_token = Some("DT-1".into());
        let srv = FixtureServer::start(vec![ScriptedResponse::usage(
            401,
            r#"{"error":"unauthorized"}"#,
        )]);
        let err = quotas_impl(&cfg, &srv.endpoints).unwrap_err().to_string();
        assert!(
            err.contains("OpenCode"),
            "the source must be named (FR-6.8): {err}"
        );
        assert!(
            err.contains("cannot refresh"),
            "llmu must not claim to refresh it: {err}"
        );
        assert!(
            !err.contains("llmu can refresh"),
            "no false refresh claim (FR-6.8)"
        );
        let reqs = srv.requests();
        assert_eq!(
            reqs.len(),
            1,
            "no refresh is ever attempted for direct tokens"
        );
        assert_eq!(reqs[0].method, "GET");
    }

    #[test]
    fn refresh_token_expiry_within_three_days_warns_without_blocking() {
        let dir = temp_dir("warn-3d");
        let mut o = oauth_valid("AT1", Some("R1"));
        o["refreshTokenExpiresAt"] = json!(now_ms() + 86_400_000);
        let path = write_oauth(&dir, &o);
        let srv = FixtureServer::start(vec![ScriptedResponse::usage_ok()]);
        let f = quotas_impl(&cfg_for(path), &srv.endpoints).unwrap();
        assert_eq!(
            f.snapshots.len(),
            3,
            "the warning must not block quota collection"
        );
        assert_eq!(f.notes.len(), 1);
        assert!(
            f.notes[0].contains("three days"),
            "warning names the horizon: {}",
            f.notes[0]
        );
    }

    #[test]
    fn expiring_token_without_refresh_token_reports_login_and_makes_no_http() {
        let dir = temp_dir("no-refresh-token");
        let path = write_oauth(&dir, &oauth_expiring("AT1", None));
        let before = fs::read(&path).unwrap();
        let srv = FixtureServer::start(vec![]);
        let err = quotas_impl(&cfg_for(path.clone()), &srv.endpoints)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("log in"),
            "no refresh token means login remediation: {err}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(
            srv.requests().is_empty(),
            "no HTTP at all without a refresh token"
        );
    }
}
