use super::Provider;
use crate::{config::Config, http, types::*};
use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};

/// Z.ai / Zhipu (bigmodel.cn) — GLM Coding Plan quotas.
///
/// The plan dashboard is backed by monitor endpoints that Zhipu does not
/// list in the public API reference but that ship in community tools
/// (robinebers/openusage, guyinwonder168/opencode-glm-quota):
///
///   GET {base}/api/monitor/usage/quota/limit    <- meters used here
///   GET {base}/api/monitor/usage/model-usage
///   GET {base}/api/monitor/usage/tool-usage
///   GET {base}/api/biz/subscription/list        <- plan name (best effort)
///
/// base = https://api.z.ai (global) or https://open.bigmodel.cn (CN).
/// Auth is the *raw* API key in `Authorization` — no `Bearer ` prefix.
pub struct Glm;

fn num(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn epoch_ms(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    num(v).and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
}

impl Provider for Glm {
    fn id(&self) -> &'static str {
        "glm"
    }
    fn configured(&self, cfg: &Config) -> bool {
        cfg.glm.key().is_some()
    }
    fn capabilities(&self) -> &'static str {
        "GLM Coding Plan quotas via /api/monitor/usage/quota/limit; per-model usage via /api/monitor/usage/model-usage (best effort)"
    }

    /// Per-model usage from the same undocumented monitor family. The
    /// response shape is unconfirmed in the wild, so parsing is
    /// shape-tolerant and failures produce an actionable note instead of
    /// silence — an LLMU_DEBUG=1 dump is enough to pin the real schema.
    fn usage(&self, cfg: &Config, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<Fetch> {
        let key = match cfg.glm.key() {
            Some(k) => k,
            None => return Ok(Fetch::default()),
        };
        let base = cfg.glm.base();
        // The endpoint validates "yyyy-MM-dd HH:mm:ss" (its own 500 message
        // says so). URL-encode space and colon.
        let fmt = |t: DateTime<Utc>| {
            t.format("%Y-%m-%d %H:%M:%S")
                .to_string()
                .replace(' ', "%20")
                .replace(':', "%3A")
        };
        let url = format!(
            "{base}/api/monitor/usage/model-usage?startTime={}&endTime={}",
            fmt(since),
            fmt(until)
        );
        let v = http::get_json(
            &url,
            &[
                ("Authorization", key.as_str()),
                ("Accept", "application/json"),
            ],
        )?;
        if v["success"].as_bool() == Some(false) {
            let msg = v["msg"].as_str().unwrap_or("");
            if msg.to_lowercase().contains("coding plan") {
                // No plan — the quotas note covers it.
                return Ok(Fetch::default());
            }
            anyhow::bail!("model-usage rejected the request: {msg}");
        }
        // Find the first array of objects under data (or root).
        let arr = ["list", "models", "usage", "records", "items"]
            .iter()
            .find_map(|k| v["data"][*k].as_array())
            .or_else(|| v["data"].as_array())
            .cloned()
            .unwrap_or_default();
        let mut events = vec![];
        for it in &arr {
            let model = ["modelName", "model", "model_name", "name"]
                .iter()
                .find_map(|k| it[*k].as_str())
                .unwrap_or("glm")
                .to_string();
            let g =
                |ks: &[&str]| -> u64 { ks.iter().find_map(|k| num(&it[*k])).unwrap_or(0.0) as u64 };
            let input = g(&[
                "inputTokens",
                "promptTokens",
                "input_tokens",
                "prompt_tokens",
            ]);
            let output = g(&[
                "outputTokens",
                "completionTokens",
                "output_tokens",
                "completion_tokens",
            ]);
            let reqs = g(&["calls", "requests", "count", "num"]);
            if input == 0 && output == 0 && reqs == 0 {
                continue;
            }
            let start = ["date", "day", "time", "statDate"]
                .iter()
                .find_map(|k| {
                    let f = &it[*k];
                    epoch_ms(f).or_else(|| {
                        f.as_str().and_then(|s| {
                            chrono::NaiveDate::parse_from_str(&s[..s.len().min(10)], "%Y-%m-%d")
                                .ok()
                                .map(|d| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
                        })
                    })
                })
                .unwrap_or(until);
            let cost = cfg.estimate_cost(&model, input, output, 0, 0);
            events.push(UsageEvent {
                provider: "glm".into(),
                source: SourceKind::Api,
                model,
                start,
                requests: reqs,
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                tool_calls: 0,
                cost_usd: cost,
                cost_is_estimate: true,
            });
        }
        if events.is_empty() && !arr.is_empty() {
            anyhow::bail!(
                "model-usage responded but no rows were parsed (unknown schema) \
                 — rerun with LLMU_DEBUG=1 and the raw dump pins the field names"
            );
        }
        Ok(Fetch {
            events,
            billed: vec![],
            ..Default::default()
        })
    }

    fn quotas(&self, cfg: &Config) -> Result<Vec<QuotaSnapshot>> {
        let key = match cfg.glm.key() {
            Some(k) => k,
            None => return Ok(vec![]),
        };
        let base = cfg.glm.base();
        let v = http::get_json(
            &format!("{base}/api/monitor/usage/quota/limit"),
            &[
                ("Authorization", key.as_str()),
                ("Accept", "application/json"),
            ],
        )?;

        // Valid key without a Coding Plan: {"success":false,"code":500,"msg":"...coding plan..."}
        if v["success"].as_bool() == Some(false) {
            let msg = v["msg"].as_str().unwrap_or("");
            if msg.to_lowercase().contains("coding plan") {
                anyhow::bail!("GLM key is valid but has no Coding Plan subscription");
            }
            anyhow::bail!("GLM quota endpoint error: {msg}");
        }

        // Plan name is best-effort; failures must not blank the meters.
        let plan = http::get_json(
            &format!("{base}/api/biz/subscription/list"),
            &[
                ("Authorization", key.as_str()),
                ("Accept", "application/json"),
            ],
        )
        .ok()
        .and_then(|s| {
            s["data"].as_array().and_then(|a| {
                a.iter()
                    .find_map(|e| e["productName"].as_str().map(String::from))
            })
        })
        .unwrap_or_else(|| "GLM Coding Plan".into());

        let limits = v["data"]["limits"]
            .as_array()
            .or_else(|| v["limits"].as_array())
            .cloned()
            .unwrap_or_default();

        let out = parse_limits(&limits, &plan);
        if out.is_empty() {
            anyhow::bail!(
                "quota endpoint responded but no limits were parsed \
                 (payload drift?) — rerun with LLMU_DEBUG=1 to see the raw response"
            );
        }
        Ok(out)
    }
}

/// Window label for CREDIT_LIMIT entries: unit 3 = hours, unit 6 = weeks
/// (observed on GLM Coding Max, 2026-08: {unit:3,number:5} = 5h session,
/// {unit:6,number:1} = weekly). Unknown units fall back to the reset-time
/// distance heuristic.
fn credit_window(e: &serde_json::Value, resets: Option<DateTime<Utc>>) -> String {
    match (e["unit"].as_u64(), e["number"].as_u64()) {
        (Some(3), Some(n)) => format!("{n}h"),
        (Some(6), Some(n)) => format!("{n}w"),
        _ => {
            let hours = resets.map(|r| (r - Utc::now()).num_hours()).unwrap_or(0);
            if hours > 24 {
                "7d".into()
            } else {
                "5h".into()
            }
        }
    }
}

pub(crate) fn parse_limits(limits: &[serde_json::Value], plan: &str) -> Vec<QuotaSnapshot> {
    let mut out = vec![];
    for e in limits {
        let ty = e["type"]
            .as_str()
            .or_else(|| e["name"].as_str())
            .unwrap_or("");
        let resets = epoch_ms(&e["nextResetTime"]);
        match ty {
            // Coding Max schema: absolute credit meters with a percentage.
            "CREDIT_LIMIT" => {
                out.push(QuotaSnapshot {
                    provider: "glm".into(),
                    plan: plan.into(),
                    window: credit_window(e, resets),
                    used: num(&e["currentValue"]).unwrap_or(0.0),
                    limit: num(&e["usage"]).unwrap_or(0.0),
                    unit: "credits".into(),
                    resets_at: resets,
                });
            }
            // Older/other plan schema: percentage meters.
            "TOKENS_LIMIT" => {
                let pct = num(&e["percentage"]).unwrap_or(0.0);
                let hours = resets.map(|r| (r - Utc::now()).num_hours()).unwrap_or(0);
                let window = if hours > 24 { "7d" } else { "5h" };
                out.push(QuotaSnapshot {
                    provider: "glm".into(),
                    plan: plan.into(),
                    window: window.into(),
                    used: pct,
                    limit: 100.0,
                    unit: "%".into(),
                    resets_at: resets,
                });
            }
            // Tool/web-search style limit: absolute used vs total.
            "TIME_LIMIT" => {
                out.push(QuotaSnapshot {
                    provider: "glm".into(),
                    plan: plan.into(),
                    window: "month".into(),
                    used: num(&e["currentValue"]).unwrap_or(0.0),
                    limit: num(&e["usage"]).unwrap_or(0.0),
                    unit: "calls".into(),
                    resets_at: resets,
                });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real GLM Coding Max payload observed 2026-08-10.
    #[test]
    fn parses_credit_limit_schema() {
        let v: serde_json::Value = serde_json::from_str(r#"{"code":200,"data":{"limits":[
          {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,"currentValue":4715,"remaining":23284,"percentage":16,"nextResetTime":1786361041347},
          {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,"currentValue":83774,"remaining":56225,"percentage":59,"nextResetTime":1786773609998}
        ],"level":"max"},"success":true}"#).unwrap();
        let limits: Vec<_> = v["data"]["limits"].as_array().unwrap().clone();
        let q = parse_limits(&limits, "GLM Coding Max");
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].window, "5h");
        assert_eq!(q[0].used, 4715.0);
        assert_eq!(q[0].limit, 28000.0);
        assert!((q[0].pct() - 16.8).abs() < 0.1);
        assert_eq!(q[1].window, "1w");
        assert!((q[1].pct() - 59.8).abs() < 0.1);
        assert!(q[0].resets_at.is_some());
    }

    #[test]
    fn credit_window_units() {
        let w = |unit: u64, number: u64| {
            credit_window(&serde_json::json!({"unit": unit, "number": number}), None)
        };
        assert_eq!(w(6, 1), "1w");
        assert_eq!(w(6, 3), "3w");
        assert_eq!(w(3, 5), "5h");
    }
}
