//! Anthropic org Admin Usage/Cost APIs (token usage + billed cost).
//!
//!   GET https://api.anthropic.com/v1/organizations/usage_report/messages
//!   GET https://api.anthropic.com/v1/organizations/cost_report
//!
//! Auth is the org Admin API key (`sk-ant-admin-...`) in `x-api-key`,
//! plus `anthropic-version: 2023-06-01` on every request. The usage
//! endpoint is fixed at `bucket_width=1d`; both paginate via
//! `has_more`/`next_page` with a `page` query param. The cost endpoint
//! 404s on some org types —
//! that break is silent by design (see `collect_billed`). Amounts may
//! arrive as numbers, decimal strings, or `{"amount": ...}`.
use super::Provider;
use crate::{config::Config, http, types::*};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

pub struct Anthropic;

const BASE: &str = "https://api.anthropic.com/v1/organizations";

fn ts(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn num(v: &serde_json::Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

/// Amounts may arrive as a number, a decimal string, or {"amount": ...}.
fn money(v: &serde_json::Value) -> f64 {
    match v {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
        serde_json::Value::Object(o) => o.get("amount").map(money).unwrap_or(0.0),
        _ => 0.0,
    }
}

impl Provider for Anthropic {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn configured(&self, cfg: &Config) -> bool {
        cfg.anthropic.key().is_some()
    }
    fn capabilities(&self) -> &'static str {
        "usage (tokens/model/day) + billed cost via Admin API; local Claude Code logs handled separately"
    }

    fn usage(&self, cfg: &Config, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<Fetch> {
        let key = cfg.anthropic.key().context("no Anthropic admin key")?;
        let hdrs: &[(&str, &str)] = &[("x-api-key", &key), ("anthropic-version", "2023-06-01")];
        let mut fetch = Fetch::default();

        // --- token usage, grouped by model, 1-day buckets, paginated ---
        let mut page: Option<String> = None;
        loop {
            let mut url = format!(
                "{BASE}/usage_report/messages?starting_at={}&ending_at={}&bucket_width=1d&group_by[]=model&limit=31",
                ts(since),
                ts(until)
            );
            if let Some(p) = &page {
                url.push_str(&format!("&page={p}"));
            }
            let v = http::get_json(&url, hdrs)?;
            for bucket in v["data"].as_array().unwrap_or(&vec![]) {
                let start = bucket["starting_at"]
                    .as_str()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or(since);
                for r in bucket["results"].as_array().unwrap_or(&vec![]) {
                    // cache-write field has appeared both flat and nested
                    let cache_write =
                        r["cache_creation_input_tokens"]
                            .as_u64()
                            .unwrap_or_else(|| {
                                r["cache_creation"]
                                    .as_object()
                                    .map(|o| o.values().filter_map(|x| x.as_u64()).sum())
                                    .unwrap_or(0)
                            });
                    let tool_calls = r["server_tool_usage"]
                        .as_object()
                        .map(|o| o.values().filter_map(|x| x.as_u64()).sum())
                        .unwrap_or(0);
                    let model = r["model"].as_str().unwrap_or("(unknown)").to_string();
                    let (input, output, cache_read) = (
                        num(&r["uncached_input_tokens"]),
                        num(&r["output_tokens"]),
                        num(&r["cache_read_input_tokens"]),
                    );
                    let cost = cfg.estimate_cost(&model, input, output, cache_read, cache_write);
                    fetch.events.push(UsageEvent {
                        provider: "anthropic".into(),
                        source: SourceKind::Api,
                        model,
                        start,
                        requests: num(&r["num_requests"]).max(num(&r["request_count"])),
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: cache_read,
                        cache_write_tokens: cache_write,
                        tool_calls,
                        cost_usd: cost,
                        cost_is_estimate: true,
                    });
                }
            }
            page = v["next_page"].as_str().map(String::from);
            if !v["has_more"].as_bool().unwrap_or(false) || page.is_none() {
                break;
            }
        }

        // --- authoritative billed cost (org-level, per day) ---
        collect_billed(&mut fetch, since, |page| {
            let mut url = format!(
                "{BASE}/cost_report?starting_at={}&ending_at={}&group_by[]=description&limit=31",
                ts(since),
                ts(until)
            );
            if let Some(p) = page {
                url.push_str(&format!("&page={p}"));
            }
            http::get_json(&url, hdrs)
        });

        Ok(fetch)
    }
}

/// Billed-cost parsing adapter: delegates pagination and error control to
/// the shared `collect_billed_pages` loop, supplying an Anthropic-schema
/// parser (RFC3339 `starting_at` buckets, `money` amounts, `description`).
fn collect_billed(
    fetch: &mut Fetch,
    since: DateTime<Utc>,
    page_source: impl FnMut(&Option<String>) -> Result<serde_json::Value>,
) {
    super::collect_billed_pages(fetch, "anthropic", page_source, |bucket, billed| {
        let start = bucket["starting_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(since);
        for r in bucket["results"].as_array().unwrap_or(&vec![]) {
            billed.push(BilledCost {
                provider: "anthropic".into(),
                start,
                amount_usd: money(&r["amount"]),
                description: r["description"].as_str().unwrap_or("").to_string(),
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn since() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn page1() -> serde_json::Value {
        serde_json::json!({
            "data": [{
                "starting_at": "2026-08-01T00:00:00Z",
                "results": [{"amount": "12.34", "description": "Token usage"}]
            }],
            "has_more": true,
            "next_page": "2"
        })
    }

    /// Scripted page source: page 1 OK (with has_more/next_page), every
    /// later page fails with HTTP `code`. Asserts the seam drives pagination.
    fn scripted(code: u16) -> impl FnMut(&Option<String>) -> Result<serde_json::Value> {
        let mut calls = 0;
        move |page: &Option<String>| {
            calls += 1;
            assert_eq!(
                page.is_some(),
                calls > 1,
                "page token must be None first, then Some"
            );
            if calls == 1 {
                Ok(page1())
            } else {
                Err(http::HttpStatusError {
                    code,
                    note: "scripted".into(),
                }
                .into())
            }
        }
    }

    #[test]
    fn collect_billed_non404_keeps_billed_and_notes() {
        let mut fetch = Fetch::default();
        collect_billed(&mut fetch, since(), scripted(403));

        assert_eq!(fetch.billed.len(), 1, "page-1 entries retained");
        assert_eq!(fetch.billed[0].start, since());
        assert_eq!(fetch.billed[0].amount_usd, 12.34);
        assert_eq!(fetch.billed[0].description, "Token usage");

        assert_eq!(fetch.notes.len(), 1, "non-404 failure pushes a note");
        let n = &fetch.notes[0];
        assert!(n.contains("anthropic"), "note names provider: {n}");
        assert!(n.contains("403"), "note carries status: {n}");
        assert!(
            n.contains("billed totals may be incomplete"),
            "note explains impact: {n}"
        );
    }

    #[test]
    fn collect_billed_404_keeps_billed_without_note() {
        let mut fetch = Fetch::default();
        collect_billed(&mut fetch, since(), scripted(404));

        assert_eq!(fetch.billed.len(), 1, "page-1 entries retained");
        assert_eq!(fetch.billed[0].amount_usd, 12.34);
        assert!(fetch.notes.is_empty(), "404 is a silent break");
    }
}
