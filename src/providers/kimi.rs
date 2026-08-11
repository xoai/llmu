use super::{FetchContext, Provider, QuotaFetch};
use crate::{config::Config, http, types::*};
use anyhow::Result;

pub struct Kimi;

impl Provider for Kimi {
    fn id(&self) -> &'static str {
        "kimi"
    }
    fn configured(&self, cfg: &Config) -> bool {
        cfg.kimi.key().is_some() || cfg.kimi.code_key().is_some()
    }
    fn capabilities(&self) -> &'static str {
        "balance via /v1/users/me/balance; Kimi For Coding weekly quota + windowed limits via api.kimi.com/coding/v1/usages"
    }

    /// "Kimi For Coding" plan meters (the `/usage` screen of kimi-cli):
    /// GET {code_base}/usages with the plan key. Numeric fields arrive as
    /// STRINGS ("limit":"100"); `usage` is the weekly meter (resetTime
    /// RFC3339), `limits[]` are windowed meters whose `window` is
    /// {duration, timeUnit: TIME_UNIT_MINUTE|HOUR|DAY|WEEK}.
    fn quotas(&self, cfg: &Config) -> Result<QuotaFetch> {
        let key = match cfg.kimi.code_key() {
            Some(k) => k,
            None => return Ok(QuotaFetch::default()),
        };
        let auth = format!("Bearer {key}");
        let v = http::get_json(
            &format!("{}/usages", cfg.kimi.code_base()),
            &[("Authorization", &auth)],
        )?;
        // Some deployments wrap the payload in `data`.
        let v = if v["data"].is_object() {
            v["data"].clone()
        } else {
            v
        };
        let out = parse_usages(&v);
        if out.is_empty() {
            anyhow::bail!(
                "coding quota endpoint responded but no meters were parsed \
                 (payload drift?) — rerun with LLMU_DEBUG=1 to see the raw response"
            );
        }
        Ok(QuotaFetch::live(out))
    }

    fn balances(&self, cfg: &Config) -> Result<Vec<BalanceSnapshot>> {
        let key = match cfg.kimi.key() {
            Some(k) => k,
            None if cfg.kimi.code_key().is_some() => anyhow::bail!(
                "wallet balance skipped — only a Kimi For Coding key was found \
                 (it serves the quota endpoint); set MOONSHOT_API_KEY for the \
                 open-platform wallet"
            ),
            None => anyhow::bail!("no Kimi/Moonshot key"),
        };
        let auth = format!("Bearer {key}");
        let url = format!("{}/v1/users/me/balance", cfg.kimi.base());
        let v = http::get_json(&url, &[("Authorization", &auth)])?;
        let d = &v["data"];
        Ok(vec![BalanceSnapshot {
            provider: "kimi".into(),
            currency: "CNY/USD (per account)".into(),
            total: d["available_balance"].as_f64().unwrap_or(0.0),
            granted: d["voucher_balance"].as_f64().unwrap_or(0.0),
            topped_up: d["cash_balance"].as_f64().unwrap_or(0.0),
        }])
    }
}

/// Number-or-numeric-string ("100" / 100 / 100.0).
fn n(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn rfc3339(v: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// "5h" / "90m" / "1w" from {duration, timeUnit: TIME_UNIT_*}.
fn window_label(w: &serde_json::Value) -> String {
    let d = w["duration"].as_u64().unwrap_or(0);
    let u = w["timeUnit"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches("TIME_UNIT_");
    match u {
        "MINUTE" if d > 0 && d % 60 == 0 => format!("{}h", d / 60),
        "MINUTE" => format!("{d}m"),
        "HOUR" => format!("{d}h"),
        "DAY" => format!("{d}d"),
        "WEEK" => format!("{d}w"),
        _ => "window".into(),
    }
}

pub(crate) fn parse_usages(v: &serde_json::Value) -> Vec<QuotaSnapshot> {
    let level = v["user"]["membership"]["level"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches("LEVEL_")
        .to_lowercase();
    let plan = if level.is_empty() {
        "Kimi For Coding".to_string()
    } else {
        format!("Kimi For Coding ({level})")
    };

    let row = |d: &serde_json::Value, window: String| -> Option<QuotaSnapshot> {
        let limit = n(&d["limit"])?;
        let used = n(&d["used"]).or_else(|| n(&d["remaining"]).map(|r| limit - r))?;
        Some(QuotaSnapshot {
            provider: "kimi".into(),
            plan: plan.clone(),
            window,
            used,
            limit,
            unit: "units".into(),
            resets_at: rfc3339(&d["resetTime"]),
        })
    };

    let mut out = vec![];
    if let Some(q) = row(&v["usage"], "7d".into()) {
        out.push(q);
    }
    for item in v["limits"].as_array().unwrap_or(&vec![]) {
        let detail = if item["detail"].is_object() {
            &item["detail"]
        } else {
            item
        };
        if let Some(q) = row(detail, window_label(&item["window"])) {
            out.push(q);
        }
    }
    out
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
            "llmu-kimi-test-{}-{}-{nonce}",
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

    const QUOTAS_OK: &str = r#"{
      "user":{"userId":"x","membership":{"level":"LEVEL_STANDARD"}},
      "usage":{"limit":"100","used":"89","remaining":"11","resetTime":"2026-08-14T07:49:49.158199Z"},
      "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
                 "detail":{"limit":"100","used":"10","remaining":"90","resetTime":"2026-08-10T14:49:49.158199Z"}}]
    }"#;
    const BALANCE_OK: &str = r#"{"data":{"available_balance":10.5,"voucher_balance":2.0,"cash_balance":8.5}}"#;

    fn ctx_with_cache(dir: &Path) -> FetchContext {
        let mut ctx = FetchContext::default();
        ctx.cache = http::CacheOptions {
            dir: Some(dir.to_path_buf()),
            ttl_seconds: 3600,
        };
        ctx
    }

    /// Real payload observed 2026-08-10 (numeric fields as strings).
    #[test]
    fn parses_real_kimi_payload() {
        let v: serde_json::Value = serde_json::from_str(r#"{
          "user":{"userId":"x","region":"REGION_OVERSEA","membership":{"level":"LEVEL_STANDARD"}},
          "usage":{"limit":"100","used":"89","remaining":"11","resetTime":"2026-08-14T07:49:49.158199Z"},
          "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
                     "detail":{"limit":"100","used":"10","remaining":"90","resetTime":"2026-08-10T14:49:49.158199Z"}}],
          "parallel":{"limit":"30"},"subType":"TYPE_PURCHASE"
        }"#).unwrap();
        let q = parse_usages(&v);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].plan, "Kimi For Coding (standard)");
        assert_eq!(q[0].window, "7d");
        assert_eq!(q[0].used, 89.0);
        assert_eq!(q[0].limit, 100.0);
        assert!(q[0].resets_at.is_some());
        assert_eq!(q[1].window, "5h");
        assert_eq!(q[1].used, 10.0);
    }

    /// Task 7 (RED): a raw TTL cache hit on the coding quota GET must
    /// never mark the emitted snapshots for last-known-good refresh
    /// (FR-3.10, AS-7): the second run is network-free and reports the
    /// marker false.
    #[test]
    fn quota_cache_hit_never_marks_last_known_good_refresh() {
        let srv = CounterServer::start(vec![(200, QUOTAS_OK)]);
        let mut cfg = Config::default();
        cfg.kimi.code_key = Some("K-1".into());
        let ctx = ctx_with_cache(&temp_dir("kimi-quota-cache"));

        let first = Kimi.quotas(&cfg, &ctx).unwrap();
        assert!(first.refresh_last_known_good);
        assert_eq!(first.snapshots.len(), 2);
        assert_eq!(srv.hits(), 1);

        let second = Kimi.quotas(&cfg, &ctx).unwrap();
        assert_eq!(second.snapshots.len(), 2);
        assert!(
            !second.refresh_last_known_good,
            "a quota cache hit must never re-age last-known-good (FR-3.10)"
        );
        assert_eq!(srv.hits(), 1, "the second run must be network-free");
    }

    /// Task 7 (RED): the wallet balance GET also opts into the raw
    /// cache — a second run within the TTL performs no network request.
    #[test]
    fn balance_get_is_cached_between_runs() {
        let srv = CounterServer::start(vec![(200, BALANCE_OK)]);
        let mut cfg = Config::default();
        cfg.kimi.api_key = Some("K-2".into());
        let ctx = ctx_with_cache(&temp_dir("kimi-balance-cache"));

        let one = Kimi.balances(&cfg, &ctx).unwrap();
        assert_eq!(one.len(), 1);
        let two = Kimi.balances(&cfg, &ctx).unwrap();
        assert_eq!(two.len(), 1);
        assert_eq!(srv.hits(), 1, "the balance GET must be cached");
    }
}
