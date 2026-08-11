use super::Provider;
use crate::config::expand_tilde;
use crate::{config::Config, types::*};
use anyhow::{Context, Result};
use chrono::{DateTime, Timelike, Utc};
use std::collections::HashMap;
use std::io::BufRead;

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
pub struct Gemini;

impl Provider for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn configured(&self, cfg: &Config) -> bool {
        cfg.gemini.usage_log.is_some()
    }
    fn capabilities(&self) -> &'static str {
        "local usageMetadata JSONL (client-side logging); billed cost lives in Google Cloud Billing"
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GeminiCfg;

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
