//! DeepSeek balance endpoint.
//!
//!   GET https://api.deepseek.com/user/balance
//!
//! Auth is `Bearer <api key>` (plus `Accept: application/json`). The
//! response carries `balance_infos[]` with `total_balance`,
//! `granted_balance`, `topped_up_balance` and `currency`. There is no
//! usage-history endpoint, so llmu snapshots the balance on every run
//! and store.rs derives daily spend from day-over-day deltas.
use super::{FetchContext, Provider};
use crate::{config::Config, http, types::*};
use anyhow::{Context, Result};

pub struct DeepSeek;

impl Provider for DeepSeek {
    fn id(&self) -> &'static str {
        "deepseek"
    }
    fn configured(&self, cfg: &Config) -> bool {
        cfg.deepseek.key_or(&["DEEPSEEK_API_KEY"]).is_some()
    }
    fn capabilities(&self) -> &'static str {
        "balance only (no usage-history API); llmu snapshots balances to derive daily spend"
    }

    fn balances(&self, cfg: &Config) -> Result<Vec<BalanceSnapshot>> {
        let key = cfg
            .deepseek
            .key_or(&["DEEPSEEK_API_KEY"])
            .context("no DeepSeek key")?;
        let auth = format!("Bearer {key}");
        let v = http::get_json(
            "https://api.deepseek.com/user/balance",
            &[("Authorization", &auth), ("Accept", "application/json")],
        )?;
        let mut out = vec![];
        for b in v["balance_infos"].as_array().unwrap_or(&vec![]) {
            let f = |k: &str| -> f64 {
                b[k].as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| b[k].as_f64())
                    .unwrap_or(0.0)
            };
            out.push(BalanceSnapshot {
                provider: "deepseek".into(),
                currency: b["currency"].as_str().unwrap_or("?").to_string(),
                total: f("total_balance"),
                granted: f("granted_balance"),
                topped_up: f("topped_up_balance"),
            });
        }
        Ok(out)
    }
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
            "llmu-deepseek-test-{}-{}-{nonce}",
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

    const BALANCE_OK: &str = r#"{"balance_infos":[
      {"currency":"USD","total_balance":"10.5","granted_balance":"2.0","topped_up_balance":"8.5"}
    ]}"#;

    /// Task 7 (RED): the balance GET opts into the raw cache — a second
    /// run within the TTL performs no network request.
    #[test]
    fn balance_get_is_cached_between_runs() {
        let srv = CounterServer::start(vec![(200, BALANCE_OK)]);
        let mut cfg = Config::default();
        cfg.deepseek.api_key = Some("D-1".into());
        let mut ctx = FetchContext::default();
        ctx.cache = http::CacheOptions {
            dir: Some(temp_dir("deepseek-cache")),
            ttl_seconds: 3600,
        };

        let one = DeepSeek.balances(&cfg, &ctx).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].total, 10.5);
        let two = DeepSeek.balances(&cfg, &ctx).unwrap();
        assert_eq!(two.len(), 1);
        assert_eq!(srv.hits(), 1, "the balance GET must be cached");
    }
}
