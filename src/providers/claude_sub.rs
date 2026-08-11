use super::Provider;
use crate::{config::Config, http, types::*};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

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
/// Token discovery: `claudeAiOauth.accessToken` inside
/// `.credentials.json` in $CLAUDE_CONFIG_DIR / ~/.claude /
/// ~/.config/claude (on macOS, Claude Code may keep it in the keychain
/// instead — point [claude].credentials at an exported copy then).
///
/// Response shape: `five_hour` / `seven_day` / `seven_day_sonnet`
/// objects with {utilization, resets_at}, plus a `limits[]` array of
/// model-scoped weekly meters (kind == "weekly_scoped").
///
/// The endpoint rate-limits aggressively; llmu calls it once per run.
/// A 401 means the token expired — running `claude` refreshes it.
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

impl Provider for ClaudeSub {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn configured(&self, cfg: &Config) -> bool {
        cfg.claude.credentials_path().is_some() || cfg.claude.access_token.is_some()
    }
    fn capabilities(&self) -> &'static str {
        "Pro/Max live session + weekly quotas via api.anthropic.com/api/oauth/usage (Claude Code OAuth token)"
    }

    fn quotas(&self, cfg: &Config) -> Result<Vec<QuotaSnapshot>> {
        // Token source: Claude Code's credentials file, else a directly
        // configured/discovered access token (e.g. OpenCode's).
        let (token, sub_type) = match cfg.claude.credentials_path() {
            Some(path) => {
                let raw = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let creds: serde_json::Value = serde_json::from_str(&raw)?;
                let t = creds["claudeAiOauth"]["accessToken"]
                    .as_str()
                    .context("no claudeAiOauth.accessToken in credentials file")?
                    .to_string();
                let s = creds["claudeAiOauth"]["subscriptionType"]
                    .as_str()
                    .map(String::from);
                (t, s)
            }
            None => match &cfg.claude.access_token {
                Some(t) => (t.clone(), None),
                None => return Ok(vec![]),
            },
        };

        let auth = format!("Bearer {token}");
        let v = http::get_json(
            "https://api.anthropic.com/api/oauth/usage",
            &[
                ("Authorization", auth.as_str()),
                ("Accept", "application/json"),
                ("anthropic-beta", "oauth-2025-04-20"),
                ("User-Agent", "llmu"),
            ],
        )
        .map_err(|e| {
            let s = e.to_string();
            if s.contains("HTTP 429") {
                anyhow::anyhow!(
                    "oauth/usage rate-limited (this endpoint throttles hard) — retry in a few minutes"
                )
            } else if s.contains("HTTP 401") {
                anyhow::anyhow!("oauth/usage 401: expired token — run `claude` once to refresh")
            } else {
                e
            }
        })?;

        let plan = sub_type.as_deref().unwrap_or("Claude subscription");

        let mut out = vec![];
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
        Ok(out)
    }
}
