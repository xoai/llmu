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

    fn balances(&self, cfg: &Config, ctx: &FetchContext) -> Result<Vec<BalanceSnapshot>> {
        let key = cfg
            .deepseek
            .key_or(&["DEEPSEEK_API_KEY"])
            .context("no DeepSeek key")?;
        let auth = format!("Bearer {key}");
        let v = http::get_json_cached(
            &ctx.cache,
            ctx.fresh,
            "https://api.deepseek.com/user/balance",
            &[("Authorization", &auth), ("Accept", "application/json")],
        )?
        .body;
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
