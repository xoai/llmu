use super::{FetchContext, Provider, QuotaFetch};
use crate::{config::Config, http, types::*};
use anyhow::Result;
use chrono::{DateTime, Duration, DurationRound, Utc};
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// OpenAI Codex CLI (ChatGPT Plus/Pro plan usage).
///
/// Two sources, no official usage API needed:
///
/// 1. LOCAL — Codex writes rollout logs to
///    `$CODEX_HOME/sessions/**/*.jsonl` (+ `archived_sessions/`).
///    `turn_context` lines carry the active model; `event_msg` lines of
///    `payload.type == "token_count"` carry CUMULATIVE
///    `info.total_token_usage` (input_tokens includes cached;
///    `cached_input_tokens` is the cache-read subset) plus the most
///    recent `rate_limits` snapshot (primary = 5h window, secondary =
///    weekly, each with used_percent / window_minutes /
///    resets_in_seconds). Per-event usage = `info.last_token_usage`
///    when present, else the delta of consecutive cumulative totals.
///
/// 2. NETWORK — the ChatGPT backend endpoint Codex itself uses:
///    `GET https://chatgpt.com/backend-api/wham/usage` with the OAuth
///    access token + account id from `$CODEX_HOME/auth.json`
///    (`tokens.access_token`, `tokens.account_id`). Returns `plan_type`
///    and `rate_limit.{primary_window,secondary_window}.used_percent`.
///
/// Neither is in OpenAI's public docs; both ship in Codex's own source
/// and community trackers (openusage, CodexBar). A 401 from wham means
/// the token expired — running `codex` refreshes it; llmu then falls
/// back to the latest rate_limits found in the session logs.
pub struct Codex;

#[derive(Default, Clone, Copy)]
struct Cum {
    input: u64,
    cached: u64,
    output: u64,
}

fn cum(v: &serde_json::Value) -> Cum {
    let g = |k: &str| v[k].as_u64().unwrap_or(0);
    Cum {
        input: g("input_tokens").max(g("prompt_tokens")),
        cached: g("cached_input_tokens").max(g("cache_read_input_tokens")),
        output: g("output_tokens").max(g("completion_tokens")),
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, since: DateTime<Utc>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out, since);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            // mtime filter: a file untouched since before the window
            // can't contain events inside it.
            let fresh = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| DateTime::<Utc>::from(t) >= since)
                .unwrap_or(true);
            if fresh {
                out.push(p);
            }
        }
    }
}

fn model_in(v: &serde_json::Value) -> Option<String> {
    for cand in [&v["model"], &v["model_name"], &v["metadata"]["model"]] {
        if let Some(s) = cand.as_str() {
            if !s.trim().is_empty() {
                return Some(s.trim().to_string());
            }
        }
    }
    None
}

/// One rate-limit window -> QuotaSnapshot ("5h" / "7d" label from its
/// window_minutes; resets from resets_in_seconds or resets_at).
fn rl_window(
    w: &serde_json::Value,
    plan: &str,
    fallback: &str,
    at: DateTime<Utc>,
) -> Option<QuotaSnapshot> {
    let used = w["used_percent"].as_f64()?;
    // Session logs use window_minutes; wham/usage uses limit_window_seconds.
    let mins = w["window_minutes"]
        .as_u64()
        .or_else(|| w["limit_window_seconds"].as_u64().map(|s| s / 60));
    let window = match mins {
        Some(m) if m <= 24 * 60 => format!("{}h", m.div_ceil(60)),
        Some(m) => format!("{}d", m / (24 * 60)),
        None => fallback.into(),
    };
    // Session logs: resets_in_seconds. wham: reset_after_seconds relative,
    // or reset_at as an epoch-seconds number.
    let resets_at = w["resets_in_seconds"]
        .as_i64()
        .or_else(|| w["reset_after_seconds"].as_i64())
        .map(|s| at + Duration::seconds(s))
        .or_else(|| {
            let ra = &w["reset_at"];
            ra.as_i64()
                .and_then(|s| chrono::TimeZone::timestamp_opt(&Utc, s, 0).single())
                .or_else(|| {
                    ra.as_str()
                        .or_else(|| w["resets_at"].as_str())
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| d.with_timezone(&Utc))
                })
        });
    Some(QuotaSnapshot {
        provider: "codex".into(),
        plan: plan.into(),
        window,
        used,
        limit: 100.0,
        unit: "%".into(),
        resets_at,
    })
}

impl Codex {
    fn session_files(&self, cfg: &Config, since: DateTime<Utc>) -> Vec<PathBuf> {
        let mut files = vec![];
        if let Some(home) = cfg.codex.home_dir() {
            for sub in ["sessions", "archived_sessions"] {
                walk(&home.join(sub), &mut files, since);
            }
        }
        files
    }

    /// (latest rate_limits line, its timestamp) across all session files.
    fn log_rate_limits(&self, cfg: &Config) -> Option<(serde_json::Value, DateTime<Utc>)> {
        let since = Utc::now() - Duration::days(8);
        let mut best: Option<(serde_json::Value, DateTime<Utc>)> = None;
        for f in self.session_files(cfg, since) {
            let Ok(file) = std::fs::File::open(&f) else {
                continue;
            };
            for line in std::io::BufReader::new(file).lines().map_while(|l| l.ok()) {
                if !line.contains("\"rate_limits\"") {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let Some(ts) = v["timestamp"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc))
                else {
                    continue;
                };
                let rl = if v["payload"]["rate_limits"].is_object() {
                    v["payload"]["rate_limits"].clone()
                } else if v["payload"]["info"]["rate_limits"].is_object() {
                    v["payload"]["info"]["rate_limits"].clone()
                } else {
                    continue;
                };
                if best.as_ref().map(|(_, t)| ts > *t).unwrap_or(true) {
                    best = Some((rl, ts));
                }
            }
        }
        best
    }
}

impl Provider for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn configured(&self, cfg: &Config) -> bool {
        cfg.codex.enabled
            && cfg
                .codex
                .home_dir()
                .map(|h| h.join("sessions").exists() || h.join("auth.json").exists())
                .unwrap_or(false)
    }
    fn capabilities(&self) -> &'static str {
        "ChatGPT-plan usage from $CODEX_HOME/sessions JSONL; 5h/weekly limits from log rate_limits + chatgpt.com wham/usage"
    }

    fn usage(
        &self,
        cfg: &Config,
        _ctx: &FetchContext,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Fetch> {
        // (Task 7: codex usage is a local JSONL read — cache-ineligible;
        // the context is accepted for trait uniformity.)
        // (hour, model) -> aggregate
        let mut agg: HashMap<(DateTime<Utc>, String), UsageEvent> = HashMap::new();

        for f in self.session_files(cfg, since) {
            let Ok(file) = std::fs::File::open(&f) else {
                continue;
            };
            let mut model = "codex".to_string();
            let mut prev = Cum::default();
            for line in std::io::BufReader::new(file).lines().map_while(|l| l.ok()) {
                let is_turn = line.contains("\"turn_context\"");
                if !is_turn && !line.contains("\"token_count\"") {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let payload = &v["payload"];
                if is_turn || v["type"].as_str() == Some("turn_context") {
                    if let Some(m) = model_in(payload).or_else(|| model_in(&v)) {
                        model = m;
                    }
                    continue;
                }
                if payload["type"].as_str() != Some("token_count") {
                    continue;
                }
                let Some(ts) = v["timestamp"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc))
                else {
                    continue;
                };

                let info = &payload["info"];
                let total = cum(&info["total_token_usage"]);
                // Per-event usage: explicit last_token_usage, else the
                // cumulative delta. Unchanged totals are re-emitted
                // stale snapshots -> skip.
                let last = &info["last_token_usage"];
                let ev = if last.is_object() {
                    cum(last)
                } else {
                    Cum {
                        input: total.input.saturating_sub(prev.input),
                        cached: total.cached.saturating_sub(prev.cached),
                        output: total.output.saturating_sub(prev.output),
                    }
                };
                prev = total;
                if ev.input == 0 && ev.output == 0 && ev.cached == 0 {
                    continue;
                }
                if ts < since || ts >= until {
                    continue;
                }

                let m = model_in(payload)
                    .or_else(|| model_in(info))
                    .unwrap_or_else(|| model.clone());
                let cached = ev.cached.min(ev.input);
                let uncached = ev.input - cached;
                let hour = ts.duration_trunc(Duration::hours(1)).unwrap_or(ts);
                let e = agg.entry((hour, m.clone())).or_insert_with(|| UsageEvent {
                    provider: "codex".into(),
                    source: SourceKind::LocalLogs,
                    model: m,
                    start: hour,
                    requests: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    tool_calls: 0,
                    cost_usd: None,
                    cost_is_estimate: true,
                });
                e.requests += 1;
                e.input_tokens += uncached;
                e.cache_read_tokens += cached;
                e.output_tokens += ev.output;
            }
        }

        let mut events: Vec<UsageEvent> = agg.into_values().collect();
        for e in &mut events {
            e.cost_usd = cfg.estimate_cost(
                &e.model,
                e.input_tokens,
                e.output_tokens,
                e.cache_read_tokens,
                e.cache_write_tokens,
            );
        }
        events.sort_by_key(|e| e.start);
        Ok(Fetch {
            events,
            billed: vec![],
            ..Default::default()
        })
    }

    fn quotas(&self, cfg: &Config, ctx: &FetchContext) -> Result<QuotaFetch> {
        // Preferred: the live wham/usage endpoint via Codex's own OAuth.
        let mut wham_err: Option<String> = None;
        if let Some(home) = cfg.codex.home_dir() {
            let auth_path = home.join("auth.json");
            if let Ok(raw) = std::fs::read_to_string(&auth_path) {
                if let Ok(a) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(token) = a["tokens"]["access_token"].as_str() {
                        let bearer = format!("Bearer {token}");
                        let mut headers: Vec<(&str, &str)> = vec![
                            ("Authorization", bearer.as_str()),
                            ("Accept", "application/json"),
                            ("User-Agent", "llmu"),
                        ];
                        let acct = a["tokens"]["account_id"].as_str().unwrap_or("");
                        if !acct.is_empty() {
                            headers.push(("ChatGPT-Account-Id", acct));
                        }
                        match http::get_json_cached(
                            &ctx.cache,
                            ctx.fresh,
                            "https://chatgpt.com/backend-api/wham/usage",
                            &headers,
                        ) {
                            Ok(cj) => {
                                let v = cj.body;
                                // A raw TTL hit must never re-age the
                                // last-known-good quota cache (FR-3.10).
                                let live = cj.origin == http::CacheOrigin::Live;
                                let plan = v["plan_type"]
                                    .as_str()
                                    .unwrap_or("ChatGPT plan")
                                    .to_string();
                                let rl = &v["rate_limit"];
                                let now = Utc::now();
                                let mut out = vec![];
                                for (key, fallback) in
                                    [("primary_window", "5h"), ("secondary_window", "7d")]
                                {
                                    if let Some(q) = rl_window(&rl[key], &plan, fallback, now) {
                                        out.push(q);
                                    }
                                }
                                // Feature-scoped meters (e.g. Codex Spark).
                                for extra in
                                    v["additional_rate_limits"].as_array().unwrap_or(&vec![])
                                {
                                    let name = extra["limit_name"].as_str().unwrap_or("feature");
                                    let label = format!("{plan} · {name}");
                                    for key in ["primary_window", "secondary_window"] {
                                        if let Some(q) =
                                            rl_window(&extra["rate_limit"][key], &label, "7d", now)
                                        {
                                            out.push(q);
                                        }
                                    }
                                }
                                if !out.is_empty() {
                                    return Ok(QuotaFetch {
                                        snapshots: out,
                                        notes: vec![],
                                        refresh_last_known_good: live,
                                    });
                                }
                                wham_err = Some(
                                    "wham/usage responded but no rate-limit windows parsed \
                                     (payload drift? LLMU_DEBUG=1 shows the raw body)"
                                        .into(),
                                );
                            }
                            Err(e) => {
                                wham_err = Some(format!(
                                    "wham/usage failed ({e}); 401 = expired token — run `codex` once to refresh"
                                ));
                            }
                        }
                    }
                }
            }
        }
        // Fallback: newest rate_limits snapshot inside the session logs.
        if let Some((rl, at)) = self.log_rate_limits(cfg) {
            let mut out = vec![];
            for (keys, fallback) in [
                (["primary", "primary_window"], "5h"),
                (["secondary", "secondary_window"], "7d"),
            ] {
                let w = keys.iter().map(|k| &rl[*k]).find(|w| w.is_object());
                if let Some(w) = w {
                    if let Some(q) = rl_window(w, "ChatGPT plan (from logs)", fallback, at) {
                        out.push(q);
                    }
                }
            }
            if !out.is_empty() {
                return Ok(QuotaFetch::live(out));
            }
        }
        // Nothing worked: say so instead of rendering nothing.
        match wham_err {
            Some(e) => anyhow::bail!("{e}; no rate_limits found in session logs either"),
            None => anyhow::bail!(
                "no auth.json token and no rate_limits in session logs \
                 — run `codex` once, or check $CODEX_HOME"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real wham/usage primary_window observed 2026-08-10.
    #[test]
    fn parses_wham_window_fields() {
        let w: serde_json::Value = serde_json::from_str(
            r#"{"used_percent":0,"limit_window_seconds":604800,"reset_after_seconds":604800,"reset_at":1786967738}"#,
        )
        .unwrap();
        let at = Utc::now();
        let q = rl_window(&w, "prolite", "5h", at).unwrap();
        assert_eq!(q.window, "7d");
        assert!(q.resets_at.is_some());
        assert_eq!(q.used, 0.0);
    }
}
