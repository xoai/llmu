use super::{Provider, QuotaFetch};
use crate::config::expand_tilde;
use crate::credentials::{self, CredentialSchema, LockTiming, ReplaceOutcome};
use crate::{config::Config, http, types::*};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Timelike, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-(hour, model) aggregation buckets: (input, output, cached, requests).
type HourlyBuckets = HashMap<(DateTime<Utc>, String), (u64, u64, u64, u64)>;

/// Google exposes Gemini API spend through Cloud Billing (console /
/// BigQuery export), not a lightweight REST usage API — so the lean
/// path is client-side: every Gemini response carries `usageMetadata`;
/// append it to a JSONL file and point `[gemini] usage_log` at it.
///
/// Expected line shape (extra fields ignored):
/// {"timestamp":"2026-08-10T03:00:00Z","model":"gemini-2.5-flash",
///  "promptTokenCount":123,"candidatesTokenCount":456,"cachedContentTokenCount":7}
///
/// Quotas (FR-5): Gemini Code Assist live quotas ride the Gemini CLI's
/// plaintext OAuth credentials (`oauth_creds.json`, FR-5.1) through
/// `loadCodeAssist` then the corrected `v1internal:retrieveUserQuota`
/// (FR-5.7). llmu never onboards accounts (FR-5.6) and never mutates
/// encrypted/keychain stores (FR-5.2).
pub struct Gemini;

/// Gemini CLI `v0.39.1` installed-app OAuth identifiers pinned from
/// `packages/core/src/code_assist/oauth2.ts` (FR-5.3). These are PUBLIC
/// upstream application identifiers, not user credentials.
const GEMINI_OAUTH_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const GEMINI_OAUTH_CLIENT_SECRET: &str = "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";

/// Refresh when the access token is absent or expires within five minutes
/// (FR-5.3).
const REFRESH_BEFORE_MS: i64 = 300_000;

/// Note for the encrypted-store marker path (FR-5.2, AS-2): a diagnostic
/// only — nothing is read or mutated.
const UNSUPPORTED_STORE_NOTE: &str = "gemini: quota: Gemini CLI encrypted/keychain credential \
storage is unsupported in this release — plaintext oauth_creds.json not found but sibling \
gemini-credentials.json exists. Run `gemini` once to export plaintext credentials or point \
[gemini] credentials at one; llmu never reads or modifies encrypted stores";

/// Internal endpoint bundle (AD-4): production paths use the pinned fixed
/// endpoints via `Default`; tests inject a local `TcpListener` server. Not
/// part of user configuration.
#[derive(Debug, Clone)]
struct GeminiEndpoints {
    token: String,
    load_code_assist: String,
    retrieve_user_quota: String,
}

impl Default for GeminiEndpoints {
    fn default() -> Self {
        GeminiEndpoints {
            token: "https://oauth2.googleapis.com/token".into(),
            load_code_assist: "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist"
                .into(),
            retrieve_user_quota: "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota"
                .into(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Epoch-millisecond `expiry_date` (FR-5.3), whatever JSON numeric form the
/// file uses.
fn expiry_ms(creds: &Value) -> Option<i64> {
    let e = &creds["expiry_date"];
    e.as_i64()
        .or_else(|| e.as_u64().and_then(|v| i64::try_from(v).ok()))
        .or_else(|| e.as_f64().map(|v| v as i64))
}

/// FR-5.3/5.4: refresh when the access token is absent or expires within
/// five minutes. An unexpired token remains usable without a refresh token.
fn needs_refresh(creds: &Value, now_ms: i64) -> bool {
    if creds["access_token"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return true;
    }
    match expiry_ms(creds) {
        Some(expiry) => expiry <= now_ms + REFRESH_BEFORE_MS,
        None => true,
    }
}

/// The stored refresh token, if any (the CAS field for the shared
/// transaction, FR-4.4).
fn gemini_refresh_token(v: &Value) -> Option<&str> {
    v["refresh_token"].as_str()
}

/// FR-4.6/FR-5.3: the merged credential must carry a nonempty access token
/// and an expiry in the future. Runs before the temp write and again after
/// the rename.
fn validate_refreshed(v: &Value, now: u64) -> Result<()> {
    if v["access_token"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        bail!("refreshed credential is missing a nonempty access_token");
    }
    let expiry = expiry_ms(v).context("refreshed credential is missing a numeric expiry_date")?;
    if expiry <= now as i64 {
        bail!("refreshed credential expiry is not in the future");
    }
    Ok(())
}

/// FR-5.3: merge only known token/expiry fields into a fresh copy, so every
/// unknown top-level and nested field survives (FR-4.2). `expires_in`
/// seconds convert to epoch milliseconds; an omitted refresh token keeps
/// the stored one.
fn merge_refresh(creds: &Value, resp: &Value, now: u64) -> Value {
    let mut m = creds.clone();
    m["access_token"] = json!(resp["access_token"].as_str().unwrap_or(""));
    if let Some(exp) = resp["expires_in"].as_u64() {
        m["expiry_date"] = json!(now.saturating_add(exp.saturating_mul(1000)));
    }
    if let Some(rt) = resp["refresh_token"].as_str() {
        if !rt.is_empty() {
            m["refresh_token"] = json!(rt);
        }
    }
    m
}

/// Token source (FR-5.3/5.4, FR-4.3): an unlocked pre-read may prove the
/// token usable and skip all lock work. Otherwise acquire the llmu refresh
/// lock and re-read under it; a token another llmu process already
/// installed is adopted. The lock is held ACROSS the refresh HTTP request,
/// then consumed by the CAS replacement — a request-before-lock hazard is
/// unrepresentable.
fn access_token(path: &Path, pre: &Value, ep: &GeminiEndpoints) -> Result<String> {
    if !needs_refresh(pre, now_ms() as i64) {
        // FR-5.4: an unexpired access token remains usable, with or
        // without a refresh token, and nothing is written.
        return pre["access_token"]
            .as_str()
            .map(String::from)
            .context("no usable access token in credential file");
    }
    let locked = credentials::lock_and_read(&LockTiming::default(), path)
        .with_context(|| format!("locking {}", path.display()))?;
    // FR-4.3: re-read after locking — skip the duplicate refresh when
    // another llmu process already installed a usable token.
    let value = locked.value();
    if !needs_refresh(value, now_ms() as i64) {
        return value["access_token"]
            .as_str()
            .map(String::from)
            .context("no usable access token in credential file");
    }
    let Some(rt) = value["refresh_token"].as_str() else {
        // FR-5.4: expired without a refresh token — only the user can
        // re-authenticate. Credentials are never mutated.
        bail!("access token is expired and the credential file has no refresh token — run `gemini` once to log in again");
    };
    let form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("client_id", GEMINI_OAUTH_CLIENT_ID),
        ("client_secret", GEMINI_OAUTH_CLIENT_SECRET),
        ("refresh_token", rt),
    ];
    // The llmu lock is held while this HTTP request is in flight: two llmu
    // processes can never use the same rotating refresh token concurrently.
    let resp = http::post_form_json(&ep.token, &[], &form).map_err(|e| {
        anyhow::anyhow!("token refresh failed ({e}) — credentials were not changed")
    })?;
    let now = now_ms();
    let validate = |v: &Value| validate_refreshed(v, now);
    let schema = CredentialSchema {
        refresh_token: &gemini_refresh_token,
        validate: &validate,
    };
    match locked.replace_with_cas(&schema, |v| merge_refresh(v, &resp, now))? {
        ReplaceOutcome::Replaced { value } => value["access_token"]
            .as_str()
            .map(String::from)
            .context("refreshed credential has no access token"),
        // A writer that ignores the llmu lock landed in the CAS window:
        // nothing was overwritten; adopt its token when it is usable.
        ReplaceOutcome::ChangedByOther { value } => {
            if !needs_refresh(&value, now_ms() as i64) {
                value["access_token"]
                    .as_str()
                    .map(String::from)
                    .context("adopted credential has no access token")
            } else {
                bail!(
                    "credentials changed during refresh by another tool — retry the quota request"
                )
            }
        }
    }
}

/// FR-5.6: specific remediation for missing onboarding, ineligible tiers,
/// validation-required, and unsupported account states — llmu never
/// onboards accounts. `effective_project` is the project that will be used
/// (API returned or configured), so a tier that requires a user-defined
/// project can be caught here.
fn check_tier(v: &Value, effective_project: Option<&str>) -> Result<()> {
    let tier = match v["currentTier"].as_object() {
        Some(t) => t,
        None => {
            if let Some(reason) = ineligibility_reason(v) {
                bail!("{reason}");
            }
            bail!("loadCodeAssist reported no usable tier — run `gemini` once to check your Code Assist access");
        }
    };
    let onboarded = tier
        .get("hasOnboardedPreviously")
        .and_then(|b| b.as_bool())
        .unwrap_or(true);
    let accepted_tos = tier
        .get("hasAcceptedTos")
        .and_then(|b| b.as_bool())
        .unwrap_or(true);
    if !onboarded || !accepted_tos {
        bail!("Code Assist onboarding is incomplete — run `gemini` once to complete it (llmu never onboards accounts)");
    }
    let needs_user_project = tier
        .get("userDefinedCloudaicompanionProject")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    if needs_user_project && effective_project.is_none() {
        bail!("your Code Assist tier requires a project — set [gemini] project or GOOGLE_CLOUD_PROJECT and retry");
    }
    Ok(())
}

/// Map known `ineligibleTiers[].reasonCode` values to secret-free,
/// action-oriented messages; unknown codes are ignored.
fn ineligibility_reason(v: &Value) -> Option<String> {
    let tiers = v["ineligibleTiers"].as_array()?;
    for t in tiers {
        let reason = t["reasonCode"].as_str().unwrap_or("UNKNOWN");
        let msg = match reason {
            "VALIDATION_REQUIRED" => {
                return Some(
                    "Code Assist requires account validation — run `gemini` or follow its validation prompt to continue"
                        .into(),
                )
            }
            "INELIGIBLE_ACCOUNT"
            | "DASHER_USER"
            | "NON_USER_ACCOUNT"
            | "RESTRICTED_AGE"
            | "RESTRICTED_NETWORK"
            | "UNKNOWN_LOCATION"
            | "UNSUPPORTED_LOCATION" => "your account is ineligible for Code Assist",
            _ => continue,
        };
        return Some(format!("{msg} ({reason}) — run `gemini` for details"));
    }
    None
}

/// Tier label for the snapshot plan field (FR-5.9): the tier name, else its
/// id, else a neutral fallback.
fn tier_label(v: &Value) -> String {
    let t = &v["currentTier"];
    t["name"]
        .as_str()
        .or_else(|| t["id"].as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("gemini")
        .to_string()
}

/// Parse one quota bucket (FR-5.8) into a `QuotaSnapshot`. `plan` is the
/// tier label; the window identifies model/token-type. `remainingFraction`
/// is the fraction of quota REMAINING in (0,1]: with a positive amount the
/// total and used amount derive from it; otherwise the fraction normalizes
/// to a 100-unit percentage. Invalid fractions clamp into [0,1]. Malformed
/// buckets are `Err` with fixed, content-free diagnostics — they are
/// reported as skips and never crash the fetch.
fn parse_bucket(plan: &str, b: &Value) -> Result<QuotaSnapshot> {
    let model = b["modelId"]
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .context("bucket has no modelId")?;
    let window = match b["tokenType"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(t) => format!("{model}:{t}"),
        None => model.to_string(),
    };
    let resets_at = match b["resetTime"].as_str() {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .map_err(|_| anyhow::anyhow!("bucket has unparseable resetTime"))?,
        None => bail!("bucket has no resetTime"),
    };
    let frac = b["remainingFraction"]
        .as_f64()
        .context("bucket has no numeric remainingFraction")?;
    if !frac.is_finite() {
        bail!("bucket has non-finite remainingFraction");
    }
    let f = frac.clamp(0.0, 1.0);
    let amount = match b.get("remainingAmount") {
        None => None,
        Some(v) => {
            let a = v
                .as_str()
                .and_then(|s| s.trim().parse::<f64>().ok())
                .or_else(|| v.as_f64());
            match a {
                Some(a) if a.is_finite() => Some(a),
                _ => bail!("bucket has non-numeric remainingAmount"),
            }
        }
    };
    let (used, limit, unit) = match amount {
        Some(a) if a > 0.0 && f > 0.0 => {
            // Derive total and used amount from remaining + fraction.
            let total = a / f;
            (total - a, total, "tokens")
        }
        _ => {
            // Normalize the remaining fraction to a 100-unit percentage.
            ((1.0 - f) * 100.0, 100.0, "%")
        }
    };
    Ok(QuotaSnapshot {
        provider: "gemini".into(),
        plan: plan.into(),
        window,
        used,
        limit,
        unit: unit.into(),
        resets_at: Some(resets_at),
    })
}

/// Deterministic row order (FR-5.9): model/window first, then tier label.
fn sort_rows(rows: &mut [QuotaSnapshot]) {
    rows.sort_by(|a, b| a.window.cmp(&b.window).then_with(|| a.plan.cmp(&b.plan)));
}

impl Provider for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn configured(&self, cfg: &Config) -> bool {
        // Existing usage behavior plus quota configuration (FR-5.2): a
        // supported plaintext credential quota-configures Gemini; the
        // encrypted marker does too, so its diagnostic is actually shown.
        cfg.gemini.usage_log.is_some()
            || cfg.gemini.credentials_path().is_some()
            || cfg.gemini.encrypted_marker_path().is_some()
    }
    fn capabilities(&self) -> &'static str {
        "local usageMetadata JSONL; Code Assist live quotas via Gemini CLI OAuth credentials (refresh-only writes)"
    }

    fn usage(&self, cfg: &Config, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<Fetch> {
        let Some(path) = &cfg.gemini.usage_log else {
            return Ok(Fetch::default());
        };
        let path = expand_tilde(path);
        let file = std::fs::File::open(&path)
            .with_context(|| format!("opening gemini usage_log at {}", path.display()))?;
        let reader = std::io::BufReader::new(file);

        // aggregate to (hour, model) buckets
        let mut agg: HourlyBuckets = HashMap::new();
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let ts = v["timestamp"]
                .as_str()
                .or_else(|| v["time"].as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc));
            let Some(ts) = ts else { continue };
            if ts < since || ts >= until {
                continue;
            }
            let usage = if v["usageMetadata"].is_object() {
                &v["usageMetadata"]
            } else {
                &v
            };
            let g = |k: &str| usage[k].as_u64().unwrap_or(0);
            let model = v["model"].as_str().unwrap_or("gemini").to_string();
            let hour = ts
                .with_minute(0)
                .unwrap()
                .with_second(0)
                .unwrap()
                .with_nanosecond(0)
                .unwrap();
            let e = agg.entry((hour, model)).or_default();
            let cached = g("cachedContentTokenCount");
            e.0 += g("promptTokenCount").saturating_sub(cached);
            e.1 += g("candidatesTokenCount") + g("thoughtsTokenCount");
            e.2 += cached;
            e.3 += 1;
        }

        let mut fetch = Fetch::default();
        for ((start, model), (input, output, cached, reqs)) in agg {
            let cost = cfg.estimate_cost(&model, input, output, cached, 0);
            fetch.events.push(UsageEvent {
                provider: "gemini".into(),
                source: SourceKind::LocalLogs,
                model,
                start,
                requests: reqs,
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cached,
                cache_write_tokens: 0,
                tool_calls: 0,
                cost_usd: cost,
                cost_is_estimate: true,
            });
        }
        Ok(fetch)
    }

    fn quotas(&self, cfg: &Config) -> Result<QuotaFetch> {
        self.quotas_with_endpoints(cfg, &GeminiEndpoints::default())
    }
}

impl Gemini {
    /// Quota flow against an endpoint bundle (AD-4). Production calls the
    /// pinned endpoints via `Provider::quotas`; tests inject local servers.
    fn quotas_with_endpoints(&self, cfg: &Config, ep: &GeminiEndpoints) -> Result<QuotaFetch> {
        let Some(path) = cfg.gemini.credentials_path() else {
            // FR-5.2 / AS-2: the encrypted-store marker is diagnosed and
            // never mutated; no network is touched.
            if cfg.gemini.encrypted_marker_path().is_some() {
                return Ok(QuotaFetch {
                    snapshots: vec![],
                    notes: vec![UNSUPPORTED_STORE_NOTE.into()],
                    refresh_last_known_good: false,
                });
            }
            return Ok(QuotaFetch::default());
        };
        let pre =
            credentials::read_json(&path).with_context(|| format!("reading {}", path.display()))?;
        let token = access_token(&path, &pre, ep)?;

        // FR-5.5: project precedence config > GOOGLE_CLOUD_PROJECT >
        // GOOGLE_CLOUD_PROJECT_ID, with a loadCodeAssist-returned project
        // authoritative. FR-5.6: blocked states bail with remediation and
        // no quota call happens.
        let configured = cfg.gemini.project_override();
        let mut body = json!({
            "metadata": {
                "ideType": "IDE_UNSPECIFIED",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI",
            }
        });
        if let Some(p) = &configured {
            body["cloudaicompanionProject"] = json!(p);
            body["metadata"]["duetProject"] = json!(p);
        }
        let auth = format!("Bearer {token}");
        let load_v = http::post_json(
            &ep.load_code_assist,
            &[
                ("Authorization", auth.as_str()),
                ("Accept", "application/json"),
                ("User-Agent", "llmu"),
            ],
            &body,
        )
        .map_err(|e| anyhow::anyhow!("loadCodeAssist failed ({e})"))?;
        let api_project = load_v["cloudaicompanionProject"]
            .as_str()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from);
        let effective = api_project.or(configured);
        check_tier(&load_v, effective.as_deref())?;
        let project = effective.context(
            "no Code Assist project available — set [gemini] project or GOOGLE_CLOUD_PROJECT, or run `gemini` once to establish one",
        )?;

        // FR-5.7: the corrected endpoint name is the only quota call.
        let v = http::post_json(
            &ep.retrieve_user_quota,
            &[
                ("Authorization", auth.as_str()),
                ("Accept", "application/json"),
                ("User-Agent", "llmu"),
            ],
            &json!({ "project": project }),
        )
        .map_err(|e| anyhow::anyhow!("retrieveUserQuota failed ({e})"))?;

        let plan = tier_label(&load_v);
        let buckets = v["buckets"].as_array().cloned().unwrap_or_default();
        let mut rows = vec![];
        let mut notes = vec![];
        for (i, b) in buckets.iter().enumerate() {
            match parse_bucket(&plan, b) {
                Ok(row) => rows.push(row),
                Err(e) => notes.push(format!(
                    "gemini: quota: skipped malformed bucket {}: {e}",
                    i + 1
                )),
            }
        }
        if rows.is_empty() {
            bail!(
                "retrieveUserQuota returned no usable quota buckets ({} bucket(s) skipped) — the Code Assist quota payload may have drifted; run `gemini` once to inspect your quota",
                buckets.len()
            );
        }
        sort_rows(&mut rows);
        // FR-5.10 / AD-1: live rows mark last-known-good for refresh.
        let mut fetch = QuotaFetch::live(rows);
        fetch.notes.extend(notes);
        Ok(fetch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GeminiCfg;
    use serde_json::json;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_DIR_NONCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = TEST_DIR_NONCE.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "llmu-gemini-test-{}-{}-{nonce}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Local HTTP fixture: serves one response per expected request and
    /// records the raw request (headers + body) for sequence assertions.
    /// All fixtures use fake tokens; no user secrets anywhere.
    struct FixtureServer {
        url: String,
        hits: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl FixtureServer {
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
                        429 => "Too Many Requests",
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
                url: format!("http://{addr}"),
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

        fn last_body(&self) -> String {
            let raw = self.requests().pop().unwrap_or_default();
            raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
        }
    }

    fn endpoints(
        token: &FixtureServer,
        load: &FixtureServer,
        quota: &FixtureServer,
    ) -> GeminiEndpoints {
        GeminiEndpoints {
            token: format!("{}/token", token.url),
            load_code_assist: format!("{}/v1internal:loadCodeAssist", load.url),
            retrieve_user_quota: format!("{}/v1internal:retrieveUserQuota", quota.url),
        }
    }

    fn write_creds(dir: &Path, obj: &serde_json::Value) -> PathBuf {
        let p = dir.join("oauth_creds.json");
        fs::write(&p, serde_json::to_vec_pretty(obj).unwrap()).unwrap();
        p
    }

    /// Config pinned to a hermetic credential path: the explicit override
    /// short-circuits $GEMINI_CLI_HOME / $HOME discovery (no real ~/.gemini
    /// is ever touched, Task 3 gotcha).
    fn cfg_with(dir: &Path) -> Config {
        Config {
            gemini: GeminiCfg {
                usage_log: None,
                credentials: Some(dir.join("oauth_creds.json")),
                project: None,
            },
            ..Default::default()
        }
    }

    fn expired_creds(extra: serde_json::Value) -> serde_json::Value {
        let mut v = json!({
            "access_token": "OLD_ACCESS",
            "refresh_token": "OLD_RT",
            "expiry_date": (now_ms() - 60_000),
            "unknown_top": {"deep": [1, 2, {"keep": "x"}]},
            "extra_field": "survive",
        });
        if let Some(obj) = extra.as_object() {
            for (k, val) in obj {
                v[k] = val.clone();
            }
        }
        v
    }

    fn fresh_creds() -> serde_json::Value {
        json!({
            "access_token": "OLD_ACCESS",
            "refresh_token": "OLD_RT",
            "expiry_date": (now_ms() + 3_600_000),
            "unknown_top": {"keep": 1},
        })
    }

    const TIER_OK: &str = r#"{"currentTier":{"id":"standard-tier","name":"Standard","hasAcceptedTos":true,"hasOnboardedPreviously":true},"cloudaicompanionProject":"projects/12345"}"#;
    const QUOTA_OK: &str = r#"{"buckets":[
        {"modelId":"gemini-code-assist","tokenType":"code","remainingAmount":"500000","remainingFraction":0.5,"resetTime":"2026-08-12T00:00:00Z"}
    ]}"#;

    // -------------------------------------------------------------------
    // parser: bucket normalization (FR-5.8)
    // -------------------------------------------------------------------

    #[test]
    fn parser_derives_total_and_used_from_amount_and_fraction() {
        let b = json!({
            "modelId": "gemini-code-assist",
            "tokenType": "code",
            "remainingAmount": "500000",
            "remainingFraction": 0.5,
            "resetTime": "2026-08-12T00:00:00Z",
        });
        let row = parse_bucket("Standard", &b).unwrap();
        assert_eq!(row.provider, "gemini");
        assert_eq!(row.plan, "Standard");
        assert_eq!(row.window, "gemini-code-assist:code");
        assert_eq!(row.used, 500_000.0, "used = limit - remaining");
        assert_eq!(row.limit, 1_000_000.0, "total = remaining / fraction");
        assert_eq!(row.unit, "tokens");
        assert_eq!(
            row.resets_at,
            Some(
                DateTime::parse_from_rfc3339("2026-08-12T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn parser_accepts_numeric_amount_strings_variants() {
        let b = json!({
            "modelId": "m", "tokenType": "chat",
            "remainingAmount": 2500, "remainingFraction": 0.25,
            "resetTime": "2026-08-12T00:00:00Z",
        });
        let row = parse_bucket("Standard", &b).unwrap();
        assert_eq!(row.limit, 10_000.0);
        assert_eq!(row.used, 7_500.0);
    }

    #[test]
    fn parser_normalizes_fraction_only_buckets_to_percent() {
        let b = json!({
            "modelId": "gemini-code-assist", "tokenType": "chat",
            "remainingFraction": 0.25, "resetTime": "2026-08-12T00:00:00Z",
        });
        let row = parse_bucket("Standard", &b).unwrap();
        assert_eq!(row.used, 75.0, "used percent = (1 - remaining) * 100");
        assert_eq!(
            row.limit, 100.0,
            "fraction-only rows normalize to a 100-unit percentage (FR-5.8)"
        );
        assert_eq!(row.unit, "%");
    }

    #[test]
    fn parser_clamps_out_of_range_fractions() {
        let over = json!({
            "modelId": "m", "remainingFraction": 1.5, "resetTime": "2026-08-12T00:00:00Z",
        });
        let row = parse_bucket("Standard", &over).unwrap();
        assert_eq!(row.used, 0.0, "fraction > 1 clamps to fully remaining");
        assert_eq!(row.limit, 100.0);

        let negative = json!({
            "modelId": "m", "remainingFraction": -0.5, "resetTime": "2026-08-12T00:00:00Z",
        });
        let row = parse_bucket("Standard", &negative).unwrap();
        assert_eq!(row.used, 100.0, "negative fraction clamps to fully used");
    }

    #[test]
    fn parser_rejects_malformed_buckets_with_fixed_diagnostics() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (
                json!({"tokenType": "code", "remainingFraction": 0.5, "resetTime": "2026-08-12T00:00:00Z"}),
                "no modelId",
            ),
            (
                json!({"modelId": "m", "remainingAmount": "not-a-number", "remainingFraction": 0.5, "resetTime": "2026-08-12T00:00:00Z"}),
                "remainingAmount",
            ),
            (
                json!({"modelId": "m", "remainingFraction": 0.5, "resetTime": "garbage"}),
                "resetTime",
            ),
            (
                json!({"modelId": "m", "remainingFraction": "NaN", "resetTime": "2026-08-12T00:00:00Z"}),
                "remainingFraction",
            ),
        ];
        for (b, needle) in cases {
            let err = parse_bucket("Standard", &b).unwrap_err().to_string();
            assert!(
                err.contains(needle),
                "malformed bucket diagnostic must name the problem, got: {err}"
            );
            let raw = b.to_string();
            assert!(
                !err.contains(&raw),
                "diagnostics must never echo bucket content (NFR Security)"
            );
        }
    }

    #[test]
    fn rows_sort_deterministically_by_window_then_plan() {
        let mk = |model: &str, frac: f64| -> QuotaSnapshot {
            parse_bucket(
                "Standard",
                &json!({
                    "modelId": model, "tokenType": "code",
                    "remainingFraction": frac, "resetTime": "2026-08-12T00:00:00Z",
                }),
            )
            .unwrap()
        };
        let mut rows = vec![
            mk("gemini-code-assist", 0.5),
            mk("gemini-chat", 0.2),
            mk("gemini-code-assist", 0.1),
        ];
        sort_rows(&mut rows);
        let windows: Vec<String> = rows.iter().map(|r| r.window.clone()).collect();
        assert_eq!(
            windows,
            vec![
                "gemini-chat:code".to_string(),
                "gemini-code-assist:code".to_string(),
                "gemini-code-assist:code".to_string(),
            ],
            "rows must sort by model/window (FR-5.9), stable within equal windows"
        );
        assert_eq!(rows[0].used, 80.0);
        assert_eq!(
            rows[1].used, 50.0,
            "stable sort keeps input order for equal keys"
        );
        assert_eq!(rows[2].used, 90.0);
    }

    // -------------------------------------------------------------------
    // token refresh decision (FR-5.3, FR-5.4)
    // -------------------------------------------------------------------

    #[test]
    fn needs_refresh_when_absent_or_within_five_minutes() {
        let now: i64 = now_ms() as i64;
        assert!(
            needs_refresh(&json!({"access_token": "a"}), now),
            "missing expiry needs refresh"
        );
        assert!(
            needs_refresh(
                &json!({"access_token": "a", "expiry_date": now + 299_000}),
                now
            ),
            "within five minutes needs refresh"
        );
        assert!(
            !needs_refresh(
                &json!({"access_token": "a", "expiry_date": now + 301_000}),
                now
            ),
            "past five minutes is usable"
        );
        assert!(
            needs_refresh(&json!({"expiry_date": now + 3_600_000}), now),
            "missing access token needs refresh"
        );
    }

    // -------------------------------------------------------------------
    // full flows against a local server (AS-1, AS-2)
    // -------------------------------------------------------------------

    #[test]
    fn refresh_flow_persists_rotated_token_and_returns_live_rows() {
        let dir = temp_dir("refresh");
        let path = write_creds(&dir, &expired_creds(json!({})));
        let token = FixtureServer::start(vec![(
            200,
            r#"{"access_token":"NEW_ACCESS","expires_in":3600}"#,
        )]);
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(200, QUOTA_OK)]);
        let ep = endpoints(&token, &load, &quota);

        let fetch = Gemini.quotas_with_endpoints(&cfg_with(&dir), &ep).unwrap();

        // sequence: exactly one refresh, one loadCodeAssist, one retrieveUserQuota
        assert_eq!(token.hits(), 1, "one refresh must occur");
        assert_eq!(load.hits(), 1);
        assert_eq!(quota.hits(), 1);

        // refresh is a form POST with the pinned installed-app constants (FR-5.3)
        let form = token.last_body();
        assert!(form.contains("grant_type=refresh_token"), "form: {form}");
        assert!(form.contains("client_id=681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"), "pinned client_id (FR-5.3), form: {form}");
        assert!(
            form.contains("client_secret=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"),
            "pinned client_secret (FR-5.3), form: {form}"
        );
        assert!(
            form.contains("refresh_token=OLD_RT"),
            "stored refresh token, form: {form}"
        );

        // corrected endpoints (FR-5.5, FR-5.7) and refreshed bearer (AS-1)
        let lreq = load.requests()[0].clone();
        assert!(
            lreq.starts_with("POST /v1internal:loadCodeAssist HTTP/1.1"),
            "loadCodeAssist first, got: {}",
            lreq.lines().next().unwrap_or("")
        );
        assert!(
            lreq.to_ascii_lowercase()
                .contains("authorization: bearer new_access"),
            "quota calls must use the refreshed token"
        );
        let qreq = quota.requests()[0].clone();
        assert!(
            qreq.starts_with("POST /v1internal:retrieveUserQuota HTTP/1.1"),
            "retrieveUserQuota second, got: {}",
            qreq.lines().next().unwrap_or("")
        );
        assert!(qreq
            .to_ascii_lowercase()
            .contains("authorization: bearer new_access"));
        let qbody: serde_json::Value = serde_json::from_str(&quota.last_body()).unwrap();
        assert_eq!(
            qbody["project"], "projects/12345",
            "API-returned project is authoritative (FR-5.5)"
        );

        // durable persistence: merged token, expiry in ms, unknown fields survive (AS-1, FR-4.2)
        let on_disk: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(on_disk["access_token"], "NEW_ACCESS");
        let expiry = on_disk["expiry_date"].as_u64().unwrap();
        assert!(
            expiry > now_ms() + 3_500_000 && expiry < now_ms() + 3_700_000,
            "expires_in converts to epoch ms (FR-5.3), got {expiry}"
        );
        assert_eq!(
            on_disk["refresh_token"], "OLD_RT",
            "old refresh token retained when response omits it (FR-5.3)"
        );
        assert_eq!(
            on_disk["unknown_top"],
            json!({"deep": [1, 2, {"keep": "x"}]}),
            "unknown fields survive (FR-4.2)"
        );
        assert_eq!(on_disk["extra_field"], "survive");
        assert!(
            !dir.join("oauth_creds.json.llmu-refresh.lock").exists(),
            "the llmu lock must be released"
        );

        // live rows, deterministically normalized (FR-5.9, AD-1)
        assert!(
            fetch.refresh_last_known_good,
            "live rows must refresh last-known-good"
        );
        assert_eq!(fetch.snapshots.len(), 1);
        assert_eq!(fetch.snapshots[0].window, "gemini-code-assist:code");
        assert_eq!(fetch.snapshots[0].plan, "Standard");
        assert_eq!(fetch.snapshots[0].used, 500_000.0);
        assert_eq!(fetch.snapshots[0].limit, 1_000_000.0);
    }

    #[cfg(unix)]
    #[test]
    fn refreshed_credential_file_stays_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let path = write_creds(&dir, &expired_creds(json!({})));
        let token = FixtureServer::start(vec![(
            200,
            r#"{"access_token":"NEW_ACCESS","expires_in":3600}"#,
        )]);
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(200, QUOTA_OK)]);
        let ep = endpoints(&token, &load, &quota);
        Gemini.quotas_with_endpoints(&cfg_with(&dir), &ep).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the credential file must stay 0600 (AS-1, FR-4.5)"
        );
    }

    #[test]
    fn refresh_rotates_refresh_token_when_response_includes_it() {
        let dir = temp_dir("rotate");
        let path = write_creds(&dir, &expired_creds(json!({})));
        let token = FixtureServer::start(vec![(
            200,
            r#"{"access_token":"NEW_ACCESS","refresh_token":"NEW_RT","expires_in":3600}"#,
        )]);
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(200, QUOTA_OK)]);
        let ep = endpoints(&token, &load, &quota);
        Gemini.quotas_with_endpoints(&cfg_with(&dir), &ep).unwrap();
        let on_disk: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk["refresh_token"], "NEW_RT",
            "a rotated refresh token must be persisted"
        );
        assert_eq!(on_disk["access_token"], "NEW_ACCESS");
    }

    #[test]
    fn usable_unexpired_token_skips_refresh_without_refresh_token() {
        let dir = temp_dir("usable");
        let path = write_creds(
            &dir,
            &json!({
                "access_token": "OLD_ACCESS",
                "expiry_date": (now_ms() + 3_600_000),
            }),
        );
        let before = fs::read(&path).unwrap();
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(200, QUOTA_OK)]);
        let token = FixtureServer::start(vec![]);
        let ep = endpoints(&token, &load, &quota);

        let fetch = Gemini.quotas_with_endpoints(&cfg_with(&dir), &ep).unwrap();

        assert_eq!(
            token.hits(),
            0,
            "an unexpired token must be usable without a refresh token (FR-5.4)"
        );
        assert!(load.requests()[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer old_access"));
        assert_eq!(fetch.snapshots.len(), 1);
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "no refresh means no credential mutation"
        );
    }

    #[test]
    fn expired_token_without_refresh_token_reports_run_gemini() {
        let dir = temp_dir("no-rt");
        write_creds(
            &dir,
            &json!({"access_token": "OLD_ACCESS", "expiry_date": (now_ms() - 60_000)}),
        );
        let token = FixtureServer::start(vec![]);
        let load = FixtureServer::start(vec![]);
        let quota = FixtureServer::start(vec![]);
        let ep = endpoints(&token, &load, &quota);
        let err = Gemini
            .quotas_with_endpoints(&cfg_with(&dir), &ep)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("run `gemini`"),
            "FR-5.4 remediation, got: {err}"
        );
        assert_eq!(
            token.hits(),
            0,
            "no refresh attempt without a refresh token"
        );
        assert_eq!(load.hits(), 0);
    }

    #[test]
    fn encrypted_marker_reports_unsupported_store_without_mutation_or_network() {
        let dir = temp_dir("encrypted");
        let marker = dir.join("gemini-credentials.json");
        let marker_bytes = b"iv:tag:encrypted-payload\n".to_vec();
        fs::write(&marker, &marker_bytes).unwrap();
        // Pin the plaintext override path (absent); the marker is its sibling.
        let cfg = cfg_with(&dir);

        let fetch = Gemini
            .quotas_with_endpoints(&cfg, &GeminiEndpoints::default())
            .unwrap();

        assert!(fetch.snapshots.is_empty());
        assert!(!fetch.refresh_last_known_good);
        let notes = fetch.notes.join(" | ");
        assert!(
            notes.contains("encrypted") && notes.contains("unsupported"),
            "unsupported-store diagnostic, got: {notes}"
        );
        assert_eq!(
            fs::read(&marker).unwrap(),
            marker_bytes,
            "unsupported stores are never mutated (FR-5.2, AS-2)"
        );
    }

    #[test]
    fn missing_credentials_return_empty_default_without_network() {
        let dir = temp_dir("none");
        let mut cfg = cfg_with(&dir);
        cfg.gemini.credentials = Some("/nonexistent/llmu-t4-absent.json".into());
        let fetch = Gemini
            .quotas_with_endpoints(&cfg, &GeminiEndpoints::default())
            .unwrap();
        assert!(fetch.snapshots.is_empty());
        assert!(fetch.notes.is_empty());
        assert!(!fetch.refresh_last_known_good);
    }

    #[test]
    fn no_onboarding_or_stale_summary_endpoint_is_ever_called() {
        let dir = temp_dir("no-onboard");
        write_creds(&dir, &expired_creds(json!({})));
        let token = FixtureServer::start(vec![(
            200,
            r#"{"access_token":"NEW_ACCESS","expires_in":3600}"#,
        )]);
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(200, QUOTA_OK)]);
        let ep = endpoints(&token, &load, &quota);
        Gemini.quotas_with_endpoints(&cfg_with(&dir), &ep).unwrap();
        for raw in load.requests().iter().chain(quota.requests().iter()) {
            assert!(
                !raw.contains("onboardUser"),
                "llmu never calls onboardUser (FR-5.6): {raw}"
            );
            assert!(
                !raw.contains("retrieveUserQuotaSummary"),
                "only the corrected endpoint may be used (FR-5.7): {raw}"
            );
        }
    }

    #[test]
    fn project_precedence_config_env_then_api_authoritative() {
        let dir = temp_dir("prec-cfg");
        write_creds(&dir, &fresh_creds());
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(200, QUOTA_OK)]);
        let token = FixtureServer::start(vec![]);
        let ep = endpoints(&token, &load, &quota);
        let mut cfg = cfg_with(&dir);
        cfg.gemini.project = Some("cfg-proj".into());
        std::env::set_var("GOOGLE_CLOUD_PROJECT", "env-proj");
        Gemini.quotas_with_endpoints(&cfg, &ep).unwrap();
        let lbody: serde_json::Value = serde_json::from_str(&load.last_body()).unwrap();
        assert_eq!(
            lbody["cloudaicompanionProject"], "cfg-proj",
            "config project wins (FR-5.5)"
        );
        let qbody: serde_json::Value = serde_json::from_str(&quota.last_body()).unwrap();
        assert_eq!(
            qbody["project"], "projects/12345",
            "an API-returned project is authoritative (FR-5.5)"
        );
        std::env::remove_var("GOOGLE_CLOUD_PROJECT");
    }

    #[test]
    fn project_precedence_google_cloud_project_over_project_id() {
        let dir = temp_dir("prec-env");
        write_creds(&dir, &fresh_creds());
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(200, QUOTA_OK)]);
        let token = FixtureServer::start(vec![]);
        let ep = endpoints(&token, &load, &quota);
        let cfg = cfg_with(&dir);
        std::env::set_var("GOOGLE_CLOUD_PROJECT", "env-proj");
        std::env::set_var("GOOGLE_CLOUD_PROJECT_ID", "id-proj");
        Gemini.quotas_with_endpoints(&cfg, &ep).unwrap();
        let lbody: serde_json::Value = serde_json::from_str(&load.last_body()).unwrap();
        assert_eq!(
            lbody["cloudaicompanionProject"], "env-proj",
            "GOOGLE_CLOUD_PROJECT beats GOOGLE_CLOUD_PROJECT_ID (FR-5.5)"
        );
        std::env::remove_var("GOOGLE_CLOUD_PROJECT");
        std::env::remove_var("GOOGLE_CLOUD_PROJECT_ID");

        let load2 = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota2 = FixtureServer::start(vec![(200, QUOTA_OK)]);
        let token2 = FixtureServer::start(vec![]);
        let ep2 = endpoints(&token2, &load2, &quota2);
        std::env::set_var("GOOGLE_CLOUD_PROJECT_ID", "id-proj");
        Gemini.quotas_with_endpoints(&cfg, &ep2).unwrap();
        let lbody2: serde_json::Value = serde_json::from_str(&load2.last_body()).unwrap();
        assert_eq!(
            lbody2["cloudaicompanionProject"], "id-proj",
            "GOOGLE_CLOUD_PROJECT_ID is the last fallback (FR-5.5)"
        );
        std::env::remove_var("GOOGLE_CLOUD_PROJECT_ID");
    }

    #[test]
    fn missing_project_with_required_tier_is_remediation_error() {
        let dir = temp_dir("proj-required");
        write_creds(&dir, &fresh_creds());
        let load = FixtureServer::start(vec![(
            200,
            r#"{"currentTier":{"id":"standard-tier","name":"Standard","hasAcceptedTos":true,"hasOnboardedPreviously":true,"userDefinedCloudaicompanionProject":true}}"#,
        )]);
        let quota = FixtureServer::start(vec![]);
        let token = FixtureServer::start(vec![]);
        let ep = endpoints(&token, &load, &quota);
        let err = Gemini
            .quotas_with_endpoints(&cfg_with(&dir), &ep)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("project"),
            "required-project remediation (FR-5.6), got: {err}"
        );
        assert_eq!(quota.hits(), 0, "no quota call without a project");
    }

    #[test]
    fn remediation_onboarding_validation_and_ineligible_states() {
        let cases: Vec<(&str, &[&str])> = vec![
            (
                r#"{"currentTier":{"id":"standard-tier","hasAcceptedTos":false,"hasOnboardedPreviously":true}}"#,
                &["onboard", "run `gemini`"],
            ),
            (
                r#"{"currentTier":{"id":"standard-tier","hasAcceptedTos":true,"hasOnboardedPreviously":false}}"#,
                &["onboard", "run `gemini`"],
            ),
            (
                r#"{"ineligibleTiers":[{"reasonCode":"VALIDATION_REQUIRED","reasonMessage":"secret-ish text"}]}"#,
                &["validation", "run `gemini`"],
            ),
            (
                r#"{"ineligibleTiers":[{"reasonCode":"INELIGIBLE_ACCOUNT"}]}"#,
                &["ineligible", "run `gemini`"],
            ),
            (r#"{}"#, &["run `gemini`"]),
        ];
        for (i, (body, needles)) in cases.iter().enumerate() {
            let dir = temp_dir(&format!("remed-{i}"));
            write_creds(&dir, &fresh_creds());
            let load = FixtureServer::start(vec![(200, body)]);
            let quota = FixtureServer::start(vec![]);
            let token = FixtureServer::start(vec![]);
            let ep = endpoints(&token, &load, &quota);
            let err = Gemini
                .quotas_with_endpoints(&cfg_with(&dir), &ep)
                .unwrap_err()
                .to_string();
            for needle in *needles {
                assert!(
                    err.contains(needle),
                    "remediation must name `{needle}`, got: {err}"
                );
            }
            assert!(
                !err.contains("secret-ish"),
                "reason messages must never leak into diagnostics"
            );
            assert_eq!(quota.hits(), 0, "no quota call from a blocked state (AS-2)");
        }
    }

    #[test]
    fn malformed_buckets_skipped_with_secret_free_notes() {
        let dir = temp_dir("malformed");
        write_creds(&dir, &fresh_creds());
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(
            200,
            r#"{"buckets":[
                {"modelId":"good","tokenType":"code","remainingFraction":0.5,"resetTime":"2026-08-12T00:00:00Z"},
                {"tokenType":"code","remainingFraction":0.5,"resetTime":"2026-08-12T00:00:00Z"},
                {"modelId":"bad-amt","remainingAmount":"not-a-number","remainingFraction":0.5,"resetTime":"2026-08-12T00:00:00Z"},
                {"modelId":"bad-time","remainingFraction":0.5,"resetTime":"sensitive-garbage"},
                {"modelId":"bad-frac","remainingFraction":"NaN","resetTime":"2026-08-12T00:00:00Z"}
            ]}"#,
        )]);
        let token = FixtureServer::start(vec![]);
        let ep = endpoints(&token, &load, &quota);

        let fetch = Gemini.quotas_with_endpoints(&cfg_with(&dir), &ep).unwrap();

        assert_eq!(fetch.snapshots.len(), 1, "only the valid bucket survives");
        assert_eq!(fetch.snapshots[0].window, "good:code");
        assert_eq!(
            fetch.notes.len(),
            4,
            "each malformed bucket gets one diagnostic"
        );
        let joined = fetch.notes.join(" | ");
        assert!(
            joined.contains("skipped malformed bucket"),
            "notes: {joined}"
        );
        assert!(
            !joined.contains("sensitive-garbage") && !joined.contains("not-a-number"),
            "skip diagnostics never echo bucket content (NFR Security): {joined}"
        );
    }

    #[test]
    fn all_malformed_buckets_yield_actionable_payload_drift_error() {
        let dir = temp_dir("all-bad");
        write_creds(&dir, &fresh_creds());
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(
            200,
            r#"{"buckets":[{"tokenType":"code","remainingFraction":0.5,"resetTime":"2026-08-12T00:00:00Z"}]}"#,
        )]);
        let token = FixtureServer::start(vec![]);
        let ep = endpoints(&token, &load, &quota);
        let err = Gemini
            .quotas_with_endpoints(&cfg_with(&dir), &ep)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("payload") && err.contains("run `gemini`"),
            "actionable payload-drift error (FR-5.8), got: {err}"
        );
    }

    #[test]
    fn api_failures_surface_for_last_known_good_fallback() {
        let dir = temp_dir("fail-500");
        write_creds(&dir, &fresh_creds());
        let load = FixtureServer::start(vec![(200, TIER_OK)]);
        let quota = FixtureServer::start(vec![(500, r#"{"error":"boom"}"#)]);
        let token = FixtureServer::start(vec![]);
        let ep = endpoints(&token, &load, &quota);
        let err = Gemini
            .quotas_with_endpoints(&cfg_with(&dir), &ep)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("retrieveUserQuota failed"),
            "quota failure degrades to the last-known-good path (FR-5.10), got: {err}"
        );
        assert!(err.contains("HTTP 500"));

        let dir2 = temp_dir("fail-load");
        write_creds(&dir2, &fresh_creds());
        let load2 = FixtureServer::start(vec![(429, r#"{"error":"throttled"}"#)]);
        let quota2 = FixtureServer::start(vec![]);
        let token2 = FixtureServer::start(vec![]);
        let ep2 = endpoints(&token2, &load2, &quota2);
        let err2 = Gemini
            .quotas_with_endpoints(&cfg_with(&dir2), &ep2)
            .unwrap_err()
            .to_string();
        assert!(
            err2.contains("loadCodeAssist failed"),
            "loadCodeAssist failure must also degrade (FR-5.10), got: {err2}"
        );
    }

    #[test]
    fn configured_requires_supported_plaintext_or_encrypted_marker() {
        let dir = temp_dir("configured");
        let mut none = Config::default();
        none.gemini.credentials = Some("/nonexistent/llmu-t4-configured.json".into());
        assert!(
            !Gemini.configured(&none),
            "no creds, no marker, no usage_log -> not configured"
        );

        let mut usage = Config::default();
        usage.gemini.credentials = Some("/nonexistent/llmu-t4-configured.json".into());
        usage.gemini.usage_log = Some(dir.join("usage.jsonl"));
        assert!(
            Gemini.configured(&usage),
            "usage_log alone keeps Gemini configured (existing behavior)"
        );

        let marker_dir = temp_dir("configured-marker");
        fs::write(marker_dir.join("gemini-credentials.json"), b"iv:tag:enc").unwrap();
        let mut marker = Config::default();
        marker.gemini.credentials = Some(marker_dir.join("oauth_creds.json"));
        assert!(
            Gemini.configured(&marker),
            "the encrypted marker still quota-configures Gemini for the diagnostic (FR-5.2)"
        );

        let cred_dir = temp_dir("configured-creds");
        write_creds(&cred_dir, &fresh_creds());
        let mut creds = Config::default();
        creds.gemini.credentials = Some(cred_dir.join("oauth_creds.json"));
        assert!(
            Gemini.configured(&creds),
            "a supported plaintext credential quota-configures Gemini (FR-5.2)"
        );
    }

    #[test]
    fn missing_usage_log_error_names_key_and_path() {
        let path = std::env::temp_dir().join(format!(
            "llmu-missing-gemini-usage-log-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Best-effort cleanup: the path should not exist, but a stale file
        // would mask the intended missing-file error path and make the
        // unwrap_err below panic.
        let _ = std::fs::remove_file(&path);
        let cfg = Config {
            gemini: GeminiCfg {
                usage_log: Some(path.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = Gemini.usage(&cfg, Utc::now(), Utc::now()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("usage_log"),
            "error must name the config key, got: {msg}"
        );
        assert!(
            msg.contains(&path.to_string_lossy().to_string()),
            "error must contain the path, got: {msg}"
        );
    }
}
