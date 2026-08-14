//! Subscription plans (Claude Pro/Max) have no public billing API, but the
//! Claude Code CLI writes per-message JSONL transcripts under
//! `~/.claude/projects/**/*.jsonl` including a `usage` block per assistant
//! message. Parsing them locally is exactly how tools like ccusage work.
//!
//! We dedupe on (message.id, requestId) because retried/continued sessions
//! can repeat entries, and we aggregate to (hour, model) buckets so the
//! 5-hour-window view stays cheap.

use crate::{config::Config, types::*};
use anyhow::Result;
use chrono::{DateTime, Duration, Timelike, Utc};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use super::Collected;

fn default_dirs() -> Vec<PathBuf> {
    let mut v = vec![];
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".claude/projects"));
        v.push(home.join(".config/claude/projects"));
    }
    v
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk_jsonl(&p, out);
        } else if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
            out.push(p);
        }
    }
}

pub fn collect(cfg: &Config, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<Collected> {
    let mut notes = vec![];
    if !cfg.claude_code.enabled {
        return Ok(Collected {
            events: vec![],
            notes,
        });
    }

    let mut dirs: Vec<PathBuf> = default_dirs();
    dirs.extend(
        cfg.claude_code
            .extra_paths
            .iter()
            .map(|p| crate::config::expand_tilde(p)),
    );
    let mut files = vec![];
    for d in &dirs {
        walk_jsonl(d, &mut files);
    }
    if files.is_empty() {
        notes.push("claude-code: no JSONL transcripts found (is Claude Code installed?)".into());
        return Ok(Collected {
            events: vec![],
            notes,
        });
    }

    let mut seen: HashSet<String> = HashSet::new();
    // (hour, model) -> (reqs, in, out, cache_r, cache_w, tools)
    let mut agg: HashMap<(DateTime<Utc>, String), [u64; 6]> = HashMap::new();

    for path in files {
        // Skip files whose last write predates the window (nothing new inside).
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                let m: DateTime<Utc> = modified.into();
                if m < since - Duration::days(1) {
                    continue;
                }
            }
        }
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            if !line.contains("\"usage\"") {
                continue; // fast path: most lines carry no usage block
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
            if ts < since || ts >= until {
                continue;
            }
            let msg = &v["message"];
            let usage = &msg["usage"];
            if !usage.is_object() {
                continue;
            }
            let model = msg["model"].as_str().unwrap_or("");
            if model.is_empty() || model == "<synthetic>" {
                continue;
            }
            // dedupe retried entries
            if let (Some(mid), Some(rid)) = (msg["id"].as_str(), v["requestId"].as_str()) {
                if !seen.insert(format!("{mid}:{rid}")) {
                    continue;
                }
            }
            let g = |k: &str| usage[k].as_u64().unwrap_or(0);
            let tools = msg["content"]
                .as_array()
                .map(|a| a.iter().filter(|b| b["type"] == "tool_use").count() as u64)
                .unwrap_or(0);
            let hour = ts
                .with_minute(0)
                .unwrap()
                .with_second(0)
                .unwrap()
                .with_nanosecond(0)
                .unwrap();
            let e = agg.entry((hour, model.to_string())).or_default();
            e[0] += 1;
            e[1] += g("input_tokens");
            e[2] += g("output_tokens");
            e[3] += g("cache_read_input_tokens");
            e[4] += g("cache_creation_input_tokens");
            e[5] += tools;
        }
    }

    let events = agg
        .into_iter()
        .map(|((start, model), m)| {
            let cost = cfg.estimate_cost(&model, m[1], m[2], m[3], m[4]);
            UsageEvent {
                provider: infer_provider(&model).into(),
                source: SourceKind::LocalLogs,
                model,
                start,
                requests: m[0],
                input_tokens: m[1],
                output_tokens: m[2],
                cache_read_tokens: m[3],
                cache_write_tokens: m[4],
                tool_calls: m[5],
                cost_usd: cost,
                cost_is_estimate: true,
            }
        })
        .collect();

    Ok(Collected { events, notes })
}

/// Rolling "last 5h" burn from local logs — a proxy for the Claude
/// subscription session window (real limits aren't exposed publicly,
/// so limit=0 → the UI shows burn without a percentage).
/// Claude Code transcripts contain whatever model the backend echoed.
/// When Claude Code is routed to another provider's Anthropic-compatible
/// endpoint (GLM / Kimi / DeepSeek coding plans via ANTHROPIC_BASE_URL),
/// the model ids are theirs — attribute those events to the real
/// provider so per-model reporting and the Claude 5h burn stay honest.
fn infer_provider(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if m.starts_with("glm") {
        "glm"
    } else if m.starts_with("deepseek") {
        "deepseek"
    } else if m.starts_with("kimi") || m.starts_with("moonshot") {
        "kimi"
    } else if m.starts_with("qwen") {
        "qwen"
    } else {
        "anthropic"
    }
}

pub fn rolling_quota(events: &[UsageEvent], now: DateTime<Utc>) -> Option<QuotaSnapshot> {
    let cutoff = now - Duration::hours(5);
    let (mut toks, mut reqs) = (0u64, 0u64);
    for e in events {
        if e.source == SourceKind::LocalLogs && e.provider == "anthropic" && e.start >= cutoff {
            toks += e.total_tokens();
            reqs += e.requests;
        }
    }
    if reqs == 0 {
        return None;
    }
    Some(QuotaSnapshot {
        provider: "anthropic".into(),
        plan: format!("Claude Code (local, {reqs} msgs)"),
        window: "5h".into(),
        used: toks as f64,
        limit: 0.0,
        unit: "tokens".into(),
        resets_at: None,
    })
}
