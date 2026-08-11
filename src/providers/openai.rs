//! OpenAI org Usage/Costs APIs (token usage + billed cost).
//!
//!   GET https://api.openai.com/v1/organization/usage/completions
//!   GET https://api.openai.com/v1/organization/costs
//!
//! Auth is `Bearer <org admin key>`. Both endpoints are fixed at
//! `bucket_width=1d` and paginate via `has_more`/`next_page` with a
//! `page` query param. Cost amounts arrive as `{"amount": {"value": ...}}`;
//! cached input tokens are counted (there is no cache-write charge). The
//! cost endpoint may be unavailable on some org types — that break is
//! silent by design (see `collect_billed`).
use super::{FetchContext, Provider};
use crate::{config::Config, http, types::*};
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};

pub struct OpenAi;

const BASE: &str = "https://api.openai.com/v1/organization";

impl Provider for OpenAi {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn configured(&self, cfg: &Config) -> bool {
        cfg.openai.key().is_some()
    }
    fn capabilities(&self) -> &'static str {
        "usage (tokens/model/day incl. cached) + billed cost via org Usage/Costs API"
    }

    fn usage(&self, cfg: &Config, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<Fetch> {
        let key = cfg.openai.key().context("no OpenAI admin key")?;
        let auth = format!("Bearer {key}");
        let hdrs: &[(&str, &str)] = &[("Authorization", &auth)];
        let mut fetch = Fetch::default();

        // --- completions usage, grouped by model, 1-day buckets ---
        let mut page: Option<String> = None;
        loop {
            let mut url = format!(
                "{BASE}/usage/completions?start_time={}&end_time={}&bucket_width=1d&group_by=model&limit=31",
                since.timestamp(),
                until.timestamp()
            );
            if let Some(p) = &page {
                url.push_str(&format!("&page={p}"));
            }
            let v = http::get_json(&url, hdrs)?;
            for bucket in v["data"].as_array().unwrap_or(&vec![]) {
                let start = bucket["start_time"]
                    .as_i64()
                    .and_then(|t| Utc.timestamp_opt(t, 0).single())
                    .unwrap_or(since);
                for r in bucket["results"].as_array().unwrap_or(&vec![]) {
                    let input_total = r["input_tokens"].as_u64().unwrap_or(0);
                    let cached = r["input_cached_tokens"].as_u64().unwrap_or(0);
                    let output = r["output_tokens"].as_u64().unwrap_or(0);
                    let model = r["model"].as_str().unwrap_or("(all models)").to_string();
                    let input = input_total.saturating_sub(cached);
                    let cost = cfg.estimate_cost(&model, input, output, cached, 0);
                    fetch.events.push(UsageEvent {
                        provider: "openai".into(),
                        source: SourceKind::Api,
                        model,
                        start,
                        requests: r["num_model_requests"].as_u64().unwrap_or(0),
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: cached,
                        cache_write_tokens: 0, // OpenAI caching has no write charge
                        tool_calls: 0,
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

        // --- authoritative billed cost, per day / line item ---
        collect_billed(&mut fetch, since, |page| {
            let mut url = format!(
                "{BASE}/costs?start_time={}&end_time={}&bucket_width=1d&group_by=line_item&limit=31",
                since.timestamp(),
                until.timestamp()
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
/// the shared `collect_billed_pages` loop, supplying an OpenAI-schema
/// parser (epoch `start_time` buckets, `amount.value`, `line_item`).
fn collect_billed(
    fetch: &mut Fetch,
    since: DateTime<Utc>,
    page_source: impl FnMut(&Option<String>) -> Result<serde_json::Value>,
) {
    super::collect_billed_pages(fetch, "openai", page_source, |bucket, billed| {
        let start = bucket["start_time"]
            .as_i64()
            .and_then(|t| Utc.timestamp_opt(t, 0).single())
            .unwrap_or(since);
        for r in bucket["results"].as_array().unwrap_or(&vec![]) {
            billed.push(BilledCost {
                provider: "openai".into(),
                start,
                amount_usd: r["amount"]["value"].as_f64().unwrap_or(0.0),
                description: r["line_item"].as_str().unwrap_or("").to_string(),
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    static TEST_DIR_NONCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = TEST_DIR_NONCE.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "llmu-openai-test-{}-{}-{nonce}",
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
    }

    impl CounterServer {
        fn start(responses: Vec<(u16, &'static str)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let hits = Arc::new(AtomicUsize::new(0));
            let h2 = hits.clone();
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
                            break;
                        }
                    }
                    h2.fetch_add(1, Ordering::SeqCst);
                    let reason = match code {
                        200 => "OK",
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
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    const USAGE_PAGE: &str = r#"{"data":[{"start_time":1754000000,"results":[
      {"model":"gpt-4o","input_tokens":20,"input_cached_tokens":5,"output_tokens":7,"num_model_requests":3}
    ]}],"has_more":false}"#;
    const COST_PAGE: &str = r#"{"data":[{"start_time":1754000000,"results":[
      {"amount":{"value":12.34},"line_item":"Tokens"}
    ]}],"has_more":false}"#;

    fn since() -> DateTime<Utc> {
        Utc.timestamp_opt(1754000000, 0).single().unwrap()
    }

    fn page1() -> serde_json::Value {
        serde_json::json!({
            "data": [{
                "start_time": 1754000000,
                "results": [{"amount": {"value": 12.34}, "line_item": "Tokens"}]
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
        assert_eq!(fetch.billed[0].description, "Tokens");

        assert_eq!(fetch.notes.len(), 1, "non-404 failure pushes a note");
        let n = &fetch.notes[0];
        assert!(n.contains("openai"), "note names provider: {n}");
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

    /// Task 7 (RED): both OpenAI GETs (usage pagination + costs report)
    /// opt into the raw cache — a second run within the TTL performs no
    /// network request and still parses the same rows.
    #[test]
    fn usage_and_cost_gets_are_cached_between_runs() {
        let srv = CounterServer::start(vec![(200, USAGE_PAGE), (200, COST_PAGE)]);
        let mut cfg = Config::default();
        cfg.openai.admin_key = Some("O-1".into());
        let mut ctx = FetchContext::default();
        ctx.cache = http::CacheOptions {
            dir: Some(temp_dir("openai-cache")),
            ttl_seconds: 3600,
        };
        let until = since() + chrono::Duration::days(1);

        let one = OpenAi.usage(&cfg, &ctx, since(), until).unwrap();
        assert_eq!(one.events.len(), 1);
        assert_eq!(one.events[0].requests, 3);
        assert_eq!(srv.hits(), 2, "usage page + costs page");

        let two = OpenAi.usage(&cfg, &ctx, since(), until).unwrap();
        assert_eq!(two.events.len(), 1);
        assert_eq!(two.billed.len(), 1);
        assert_eq!(srv.hits(), 2, "both GETs must hit the raw cache");
    }
}
