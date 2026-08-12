use super::{FetchContext, Provider};
use crate::{config::Config, types::*};
use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// Qwen Cloud / Alibaba Model Studio (local read-only usage).
///
/// Reads Qwen Code's request-level token ledger
/// (`${runtime}/usage/token-usage-YYYY-MM.jsonl`, FR-3) and the legacy
/// session-summary ledger (`${home}/usage_record.jsonl`, FR-4) with no
/// network calls and no file writes. Diagnostics are aggregated: at most
/// one skipped-record note and one unreadable-file note per `usage()` call,
/// and neither ever carries a path, record body, key, token, or settings
/// value (FR-5).
pub struct Qwen;

/// One accepted request-ledger record (`tokenUsageService.ts`, v1).
#[derive(Debug, Clone)]
struct RequestRecord {
    id: String,
    session: String,
    ts: DateTime<Utc>,
    model: String,
    input: u64,
    output: u64,
    cached: u64,
    thoughts: u64,
}

/// One accepted legacy per-model entry (`usageHistoryService.ts`, v1).
#[derive(Debug, Clone)]
struct LegacyEntry {
    model: String,
    requests: u64,
    input: u64,
    output: u64,
    cached: u64,
    thoughts: u64,
}

/// One accepted legacy session-summary record.
#[derive(Debug, Clone)]
struct LegacyRecord {
    session: String,
    ts: DateTime<Utc>,
    models: Vec<LegacyEntry>,
}

/// serde_json numbers must be finite, nonnegative, and integer-compatible
/// for u64: `as_u64` rejects negative, fractional, nonnumeric, and
/// overflowing values (JSON text can never produce a non-finite number).
/// An invalid field rejects the whole record — it is never defaulted to
/// zero (FR-3).
fn u64_field(v: &serde_json::Value, key: &str) -> Option<u64> {
    v[key].as_u64()
}

/// Qwen-family model attribution: `qwen` prefix, ASCII case-insensitive.
/// Non-Qwen models are silently filtered, never attributed to qwen.
fn is_qwen_model(model: &str) -> bool {
    let m = model.trim();
    !m.is_empty() && m.to_ascii_lowercase().starts_with("qwen")
}

fn parse_request(v: &serde_json::Value) -> Option<RequestRecord> {
    if v["schemaVersion"].as_u64() != Some(1) {
        return None;
    }
    let id = v["id"].as_str()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let session = v["sessionId"].as_str()?.trim().to_string();
    if session.is_empty() {
        return None;
    }
    let ts = DateTime::parse_from_rfc3339(v["timestamp"].as_str()?)
        .ok()?
        .with_timezone(&Utc);
    let model = v["model"].as_str()?.trim().to_string();
    let input = u64_field(v, "inputTokens")?;
    let output = u64_field(v, "outputTokens")?;
    let cached = u64_field(v, "cachedTokens")?;
    let thoughts = u64_field(v, "thoughtsTokens")?;
    u64_field(v, "totalTokens")?; // validation/context only (FR-3)
    Some(RequestRecord {
        id,
        session,
        ts,
        model,
        input,
        output,
        cached,
        thoughts,
    })
}

fn parse_legacy_entry(model: &str, m: &serde_json::Value) -> Option<LegacyEntry> {
    let requests = u64_field(m, "requests")?;
    let input = u64_field(m, "inputTokens")?;
    let output = u64_field(m, "outputTokens")?;
    let cached = u64_field(m, "cachedTokens")?;
    let thoughts = u64_field(m, "thoughtsTokens")?;
    u64_field(m, "totalTokens")?;
    Some(LegacyEntry {
        model: model.to_string(),
        requests,
        input,
        output,
        cached,
        thoughts,
    })
}

/// One accepted legacy session-summary record plus the number of malformed
/// Qwen model entries skipped inside it. Non-Qwen models are filtered
/// silently and never counted as malformed (FR-4); an invalid Qwen model
/// entry counts toward the aggregate skipped-record note (FR-5).
fn parse_legacy(v: &serde_json::Value) -> Option<(LegacyRecord, usize)> {
    if v["version"].as_u64() != Some(1) {
        return None;
    }
    let session = v["sessionId"].as_str()?.trim().to_string();
    if session.is_empty() {
        return None;
    }
    let ms = i64::try_from(u64_field(v, "timestamp")?).ok()?;
    let ts = Utc.timestamp_millis_opt(ms).single()?;
    let mut accepted = vec![];
    let mut malformed = 0usize;
    for (model, m) in v["models"].as_object()? {
        if !is_qwen_model(model) {
            continue;
        }
        if let Some(e) = parse_legacy_entry(model, m) {
            accepted.push(e);
        } else {
            malformed += 1;
        }
    }
    Some((
        LegacyRecord {
            session,
            ts,
            models: accepted,
        },
        malformed,
    ))
}

/// Every `usage/token-usage-*.jsonl` entry under the runtime directory in
/// lexicographic path order (FR-3: local-month filenames, not UTC months).
/// Non-regular entries (e.g. a directory at a file path) are included so
/// they surface as unreadable files instead of disappearing.
fn request_files(runtime: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(runtime.join("usage")) else {
        return vec![];
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("token-usage-") && n.ends_with(".jsonl"))
        })
        .collect();
    files.sort();
    files
}

fn has_request_file(runtime: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(runtime.join("usage")) else {
        return false;
    };
    rd.flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("token-usage-") && n.ends_with(".jsonl"))
            && e.path().is_file()
    })
}

/// (Qwen home, effective runtime dir). The legacy ledger lives only under
/// `cfg.qwen.home` (FR-4); an absent home never falls back to the runtime
/// directory. The runtime directory may fall back to home when no runtime
/// override is set (FR-1). After discovery both are installed; this only
/// distinguishes explicit config, where the two may diverge.
fn effective_paths(cfg: &Config) -> (Option<PathBuf>, Option<PathBuf>) {
    let home = cfg.qwen.home.clone();
    let runtime = cfg.qwen.runtime_dir.clone().or(home.clone());
    (home, runtime)
}

impl Provider for Qwen {
    fn id(&self) -> &'static str {
        "qwen"
    }

    /// FR-6: configured when any credential class is present or a
    /// supported local usage file exists.
    fn configured(&self, cfg: &Config) -> bool {
        let has_key = cfg
            .qwen
            .standard_key
            .as_deref()
            .is_some_and(|k| !k.is_empty())
            || cfg
                .qwen
                .coding_plan_key
                .as_deref()
                .is_some_and(|k| !k.is_empty())
            || cfg
                .qwen
                .token_plan_key
                .as_deref()
                .is_some_and(|k| !k.is_empty());
        if has_key {
            return true;
        }
        let (home, runtime) = effective_paths(cfg);
        let legacy = home
            .as_ref()
            .is_some_and(|h| h.join("usage_record.jsonl").is_file());
        legacy || runtime.as_ref().is_some_and(|r| has_request_file(r))
    }

    fn capabilities(&self) -> &'static str {
        "Qwen Code local usage records (request ledger + legacy summaries); QwenCloud account quota/billing is console-only"
    }

    fn usage(
        &self,
        cfg: &Config,
        _ctx: &FetchContext,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Fetch> {
        let (home, runtime) = effective_paths(cfg);
        let mut skipped = 0usize;
        let mut unreadable = 0usize;
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut covered_sessions: HashSet<String> = HashSet::new();
        let mut out: Vec<(DateTime<Utc>, String, String, UsageEvent)> = vec![];

        if let Some(runtime) = runtime {
            for f in request_files(&runtime) {
                let file = match std::fs::File::open(&f) {
                    Ok(file) => file,
                    Err(_) => {
                        unreadable += 1;
                        continue;
                    }
                };
                for line in std::io::BufReader::new(file).lines() {
                    let line = match line {
                        Ok(line) => line,
                        Err(_) => {
                            unreadable += 1;
                            break;
                        }
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                        skipped += 1;
                        continue;
                    };
                    let Some(rec) = parse_request(&v) else {
                        skipped += 1;
                        continue;
                    };
                    if !is_qwen_model(&rec.model) {
                        continue;
                    }
                    // Dedup by stable id (first occurrence wins); any
                    // accepted record marks its session as request-covered
                    // (FR-4), even when the record is outside the window.
                    if !seen_ids.insert(rec.id.clone()) {
                        continue;
                    }
                    covered_sessions.insert(rec.session.clone());
                    if rec.ts < since || rec.ts >= until {
                        continue;
                    }
                    let uncached = rec.input.saturating_sub(rec.cached);
                    let output = rec.output.saturating_add(rec.thoughts);
                    // Cost estimated only when the user's pricing table
                    // matches the model (FR-3); no built-in Qwen price
                    // guesses exist.
                    let cost_usd = cfg.estimate_cost(&rec.model, uncached, output, rec.cached, 0);
                    out.push((
                        rec.ts,
                        rec.model.clone(),
                        rec.id,
                        UsageEvent {
                            provider: "qwen".into(),
                            source: SourceKind::LocalLogs,
                            model: rec.model,
                            start: rec.ts,
                            requests: 1,
                            input_tokens: uncached,
                            output_tokens: output,
                            cache_read_tokens: rec.cached,
                            cache_write_tokens: 0,
                            tool_calls: 0,
                            cost_usd,
                            cost_is_estimate: true,
                        },
                    ));
                }
            }
        }

        // Legacy ledger: last valid record wins per session (upstream
        // last-wins repair), then request-covered sessions are skipped.
        let mut last: HashMap<String, LegacyRecord> = HashMap::new();
        if let Some(home) = home {
            let p = home.join("usage_record.jsonl");
            let file = match std::fs::File::open(&p) {
                Ok(file) => Some(file),
                Err(_) if p.exists() => {
                    unreadable += 1;
                    None
                }
                Err(_) => None, // missing: no local usage, not an error (FR-5)
            };
            if let Some(file) = file {
                for line in std::io::BufReader::new(file).lines() {
                    let line = match line {
                        Ok(line) => line,
                        Err(_) => {
                            unreadable += 1;
                            break;
                        }
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                        skipped += 1;
                        continue;
                    };
                    let Some((rec, malformed)) = parse_legacy(&v) else {
                        skipped += 1;
                        continue;
                    };
                    skipped += malformed;
                    last.insert(rec.session.clone(), rec);
                }
            }
        }
        for rec in last.into_values() {
            if covered_sessions.contains(&rec.session) {
                continue;
            }
            // The same [since, until) window as request records, applied
            // after last-wins/session suppression and before emission
            // (FR-3/FR-4).
            if rec.ts < since || rec.ts >= until {
                continue;
            }
            for e in rec.models {
                let uncached = e.input.saturating_sub(e.cached);
                let output = e.output.saturating_add(e.thoughts);
                let cost_usd = cfg.estimate_cost(&e.model, uncached, output, e.cached, 0);
                out.push((
                    rec.ts,
                    e.model.clone(),
                    rec.session.clone(),
                    UsageEvent {
                        provider: "qwen".into(),
                        source: SourceKind::LocalLogs,
                        model: e.model,
                        start: rec.ts,
                        requests: e.requests,
                        input_tokens: uncached,
                        output_tokens: output,
                        cache_read_tokens: e.cached,
                        cache_write_tokens: 0,
                        tool_calls: 0,
                        cost_usd,
                        cost_is_estimate: true,
                    },
                ));
            }
        }

        // Deterministic order (FR-3/FR-4): timestamp, model id, then the
        // stable request/session id.
        out.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        let events = out.into_iter().map(|(_, _, _, e)| e).collect();
        let mut notes = vec![];
        if skipped > 0 {
            notes.push(format!(
                "qwen: skipped {skipped} malformed local usage record(s)"
            ));
        }
        if unreadable > 0 {
            notes.push(format!(
                "qwen: skipped {unreadable} unreadable local usage file(s)"
            ));
        }
        Ok(Fetch {
            events,
            notes,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SINCE: &str = "2026-08-01T00:00:00Z";
    const UNTIL: &str = "2026-09-01T00:00:00Z";

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "llmu-qwen-provider-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Hermetic config: explicit temp home/runtime, no discovery, no env.
    fn cfg_with(home: &Path, runtime: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.qwen.home = Some(home.to_path_buf());
        cfg.qwen.runtime_dir = Some(runtime.to_path_buf());
        cfg
    }

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn write_lines(dir: &Path, name: &str, lines: &[&str]) {
        write(&dir.join(name), &(lines.join("\n") + "\n"));
    }

    /// One well-formed request-ledger line (upstream tokenUsageService.ts).
    #[allow(clippy::too_many_arguments)]
    fn req(
        id: &str,
        session: &str,
        ts: &str,
        model: &str,
        input: u64,
        output: u64,
        cached: u64,
        thoughts: u64,
        total: u64,
    ) -> String {
        json!({
            "schemaVersion": 1,
            "id": id,
            "timestamp": ts,
            "localDate": "2026-08-05",
            "localMonth": "2026-08",
            "sessionId": session,
            "model": model,
            "authType": "dashscope",
            "source": "api",
            "inputTokens": input,
            "outputTokens": output,
            "cachedTokens": cached,
            "thoughtsTokens": thoughts,
            "totalTokens": total,
            "apiDurationMs": 1234
        })
        .to_string()
    }

    /// One well-formed legacy line (upstream usageHistoryService.ts).
    #[allow(clippy::too_many_arguments)]
    fn legacy(
        session: &str,
        ts_ms: i64,
        models: &[(&str, u64, u64, u64, u64, u64, u64)],
    ) -> String {
        let mut m = serde_json::Map::new();
        for (model, reqs, input, output, cached, thoughts, total) in models {
            m.insert(
                model.to_string(),
                json!({
                    "requests": reqs,
                    "inputTokens": input,
                    "outputTokens": output,
                    "cachedTokens": cached,
                    "thoughtsTokens": thoughts,
                    "totalTokens": total
                }),
            );
        }
        json!({
            "version": 1,
            "sessionId": session,
            "timestamp": ts_ms,
            "startTime": ts_ms,
            "durationMs": 1200,
            "models": m
        })
        .to_string()
    }

    fn usage(cfg: &Config) -> Fetch {
        Qwen.usage(cfg, &FetchContext::default(), dt(SINCE), dt(UNTIL))
            .unwrap()
    }

    fn row(e: &UsageEvent) -> (String, String, u64, u64, u64, u64, u64) {
        (
            e.start.to_rfc3339(),
            e.model.clone(),
            e.requests,
            e.input_tokens,
            e.output_tokens,
            e.cache_read_tokens,
            e.cache_write_tokens,
        )
    }

    /// AC-3: every regular request file is enumerated in lexicographic path
    /// order (adjacent local months included), records are filtered by UTC
    /// timestamp against [since, until), and events are deterministic.
    #[test]
    fn all_files_lexicographic_with_adjacent_local_months() {
        let dir = temp_dir("all-files");
        let home = dir.join("home");
        let runtime = dir.join("runtime");
        let cfg = cfg_with(&home, &runtime);
        write_lines(
            &runtime.join("usage"),
            "token-usage-2026-07.jsonl",
            &[
                // UTC month 08 inside the 2026-07 file: the writer's local
                // month named the file, so enumeration must not derive
                // files from UTC months (AC-3).
                &req(
                    "id-jul",
                    "s-jul",
                    "2026-08-01T00:30:00Z",
                    "qwen-max",
                    20,
                    20,
                    0,
                    0,
                    40,
                ),
                &req(
                    "id-jul-out",
                    "s-jul-out",
                    "2026-07-31T23:59:59Z",
                    "qwen-max",
                    100,
                    100,
                    0,
                    0,
                    200,
                ),
            ],
        );
        write_lines(
            &runtime.join("usage"),
            "token-usage-2026-08.jsonl",
            &[
                &req(
                    "id-a",
                    "s-a",
                    "2026-08-01T00:00:00Z",
                    "qwen-max",
                    1500,
                    800,
                    1000,
                    200,
                    2500,
                ),
                &req(
                    "id-b",
                    "s-b",
                    "2026-08-31T23:30:00Z",
                    "Qwen-Plus",
                    300,
                    50,
                    300,
                    0,
                    350,
                ),
                // UTC month 09 inside the 2026-08 file and outside
                // [since, until): enumerated, then range-filtered.
                &req(
                    "id-c",
                    "s-c",
                    "2026-09-01T00:10:00Z",
                    "qwen-turbo",
                    10,
                    20,
                    0,
                    0,
                    30,
                ),
            ],
        );
        write_lines(
            &runtime.join("usage"),
            "token-usage-2026-09.jsonl",
            &[&req(
                "id-d",
                "s-d",
                "2026-09-05T00:00:00Z",
                "qwen-max",
                10,
                20,
                0,
                0,
                30,
            )],
        );
        let f = usage(&cfg);
        let rows: Vec<_> = f.events.iter().map(row).collect();
        assert_eq!(f.notes, Vec::<String>::new(), "no notes expected");
        assert_eq!(
            rows,
            vec![
                (
                    "2026-08-01T00:00:00+00:00".into(),
                    "qwen-max".into(),
                    1,
                    500,
                    1000,
                    1000,
                    0
                ),
                (
                    "2026-08-01T00:30:00+00:00".into(),
                    "qwen-max".into(),
                    1,
                    20,
                    20,
                    0,
                    0
                ),
                (
                    "2026-08-31T23:30:00+00:00".into(),
                    "Qwen-Plus".into(),
                    1,
                    0,
                    50,
                    300,
                    0
                ),
            ]
        );
        for e in &f.events {
            assert_eq!(e.provider, "qwen");
            assert_eq!(e.source, SourceKind::LocalLogs);
            assert_eq!(e.cache_write_tokens, 0);
            assert_eq!(e.tool_calls, 0);
        }
    }

    /// FR-3: [since, until) — `since` included, `until` excluded.
    #[test]
    fn rfc3339_range_filter_bounds() {
        let dir = temp_dir("range");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write_lines(
            &dir.join("runtime/usage"),
            "token-usage-2026-08.jsonl",
            &[
                &req(
                    "e1",
                    "s1",
                    "2026-07-31T23:59:59.999Z",
                    "qwen-max",
                    1,
                    1,
                    0,
                    0,
                    2,
                ),
                &req(
                    "e2",
                    "s2",
                    "2026-08-01T00:00:00.000Z",
                    "qwen-max",
                    1,
                    1,
                    0,
                    0,
                    2,
                ),
                &req(
                    "e3",
                    "s3",
                    "2026-08-31T23:59:59.999Z",
                    "qwen-max",
                    1,
                    1,
                    0,
                    0,
                    2,
                ),
                &req(
                    "e4",
                    "s4",
                    "2026-09-01T00:00:00.000Z",
                    "qwen-max",
                    1,
                    1,
                    0,
                    0,
                    2,
                ),
            ],
        );
        let f = usage(&cfg);
        let ids: Vec<_> = f.events.iter().map(|e| e.requests).collect();
        assert_eq!(f.events.len(), 2, "only in-window records emit");
        assert_eq!(ids, vec![1, 1]);
        assert!(f.notes.is_empty());
    }

    /// FR-3: request records dedup by stable id; first file (lexicographic
    /// path order) wins.
    #[test]
    fn request_ids_dedup_across_files_first_wins() {
        let dir = temp_dir("dedup");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write_lines(
            &dir.join("runtime/usage"),
            "token-usage-2026-08.jsonl",
            &[&req(
                "dup",
                "s-dup",
                "2026-08-10T00:00:00Z",
                "qwen-max",
                100,
                1,
                0,
                0,
                101,
            )],
        );
        write_lines(
            &dir.join("runtime/usage"),
            "token-usage-2026-09.jsonl",
            &[
                &req(
                    "dup",
                    "s-dup2",
                    "2026-08-20T00:00:00Z",
                    "qwen-max",
                    999,
                    1,
                    0,
                    0,
                    1000,
                ),
                &req(
                    "solo",
                    "s-solo",
                    "2026-08-21T00:00:00Z",
                    "qwen-max",
                    7,
                    1,
                    0,
                    0,
                    8,
                ),
            ],
        );
        let f = usage(&cfg);
        assert_eq!(f.events.len(), 2, "duplicate id must not double count");
        let input: Vec<_> = f.events.iter().map(|e| e.input_tokens).collect();
        assert_eq!(
            input,
            vec![100, 7],
            "dedup keeps the first occurrence (lexicographic file); events stay time-ordered"
        );
        assert!(f.notes.is_empty(), "dedup is silent");
    }

    /// FR-3: only Qwen-family models emit; prefix match is ASCII
    /// case-insensitive; non-Qwen models are filtered without diagnostics.
    #[test]
    fn qwen_only_model_filtering_is_case_insensitive() {
        let dir = temp_dir("models");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write_lines(
            &dir.join("runtime/usage"),
            "token-usage-2026-08.jsonl",
            &[
                &req(
                    "m1",
                    "s1",
                    "2026-08-02T00:00:00Z",
                    "qwen-max",
                    1,
                    1,
                    0,
                    0,
                    2,
                ),
                &req(
                    "m2",
                    "s2",
                    "2026-08-03T00:00:00Z",
                    "QWEN-PLUS",
                    1,
                    1,
                    0,
                    0,
                    2,
                ),
                &req("m3", "s3", "2026-08-04T00:00:00Z", "gpt-4o", 1, 1, 0, 0, 2),
                &req(
                    "m4",
                    "s4",
                    "2026-08-05T00:00:00Z",
                    "deepseek-chat",
                    1,
                    1,
                    0,
                    0,
                    2,
                ),
                &req("m5", "s5", "2026-08-06T00:00:00Z", "", 1, 1, 0, 0, 2),
            ],
        );
        let f = usage(&cfg);
        let models: Vec<_> = f.events.iter().map(|e| e.model.clone()).collect();
        assert_eq!(
            models,
            vec!["qwen-max".to_string(), "QWEN-PLUS".to_string()]
        );
        assert!(f.notes.is_empty(), "model filtering is silent");
    }

    /// FR-3/AC-3: cached tokens are a subset of input (uncached =
    /// input - cached, including input == cached -> 0) and thoughts fold
    /// into output; totalTokens is never a separate bucket.
    #[test]
    fn cached_input_and_thinking_normalization() {
        let dir = temp_dir("normalize");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write_lines(
            &dir.join("runtime/usage"),
            "token-usage-2026-08.jsonl",
            &[
                &req(
                    "n1",
                    "s1",
                    "2026-08-02T00:00:00Z",
                    "qwen-max",
                    1500,
                    800,
                    1000,
                    200,
                    2500,
                ),
                &req(
                    "n2",
                    "s2",
                    "2026-08-03T00:00:00Z",
                    "qwen-max",
                    300,
                    50,
                    300,
                    0,
                    350,
                ),
            ],
        );
        let f = usage(&cfg);
        let rows: Vec<_> = f.events.iter().map(row).collect();
        assert_eq!(
            rows,
            vec![
                (
                    "2026-08-02T00:00:00+00:00".into(),
                    "qwen-max".into(),
                    1,
                    500,
                    1000,
                    1000,
                    0
                ),
                (
                    "2026-08-03T00:00:00+00:00".into(),
                    "qwen-max".into(),
                    1,
                    0,
                    50,
                    300,
                    0
                ),
            ]
        );
        assert_eq!(
            f.events[0].total_tokens(),
            2500,
            "uncached input + output + cache read only (FR-3 no double count)"
        );
    }

    /// FR-3: normalization arithmetic saturates instead of overflowing.
    #[test]
    fn saturating_overflow_on_normalization() {
        let dir = temp_dir("saturate");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write_lines(
            &dir.join("runtime/usage"),
            "token-usage-2026-08.jsonl",
            &[
                &req(
                    "o1",
                    "s1",
                    "2026-08-02T00:00:00Z",
                    "qwen-max",
                    u64::MAX,
                    u64::MAX,
                    10,
                    100,
                    u64::MAX,
                ),
                &req(
                    "o2",
                    "s2",
                    "2026-08-03T00:00:00Z",
                    "qwen-max",
                    u64::MAX,
                    u64::MAX,
                    u64::MAX,
                    5,
                    u64::MAX,
                ),
            ],
        );
        write(
            &dir.join("home/usage_record.jsonl"),
            &format!(
                "{}\n",
                legacy(
                    "s-leg",
                    dt("2026-08-04T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 1, u64::MAX, u64::MAX, u64::MAX, 5, u64::MAX)],
                )
            ),
        );
        let f = usage(&cfg);
        let rows: Vec<_> = f.events.iter().map(row).collect();
        assert_eq!(
            rows,
            vec![
                // input MAX - 10; output MAX + 100 saturates at MAX.
                (
                    "2026-08-02T00:00:00+00:00".into(),
                    "qwen-max".into(),
                    1,
                    u64::MAX - 10,
                    u64::MAX,
                    10,
                    0
                ),
                // input MAX - MAX = 0; output MAX + 5 saturates at MAX.
                (
                    "2026-08-03T00:00:00+00:00".into(),
                    "qwen-max".into(),
                    1,
                    0,
                    u64::MAX,
                    u64::MAX,
                    0
                ),
                // legacy: same saturating semantics.
                (
                    "2026-08-04T00:00:00+00:00".into(),
                    "qwen-max".into(),
                    1,
                    0,
                    u64::MAX,
                    u64::MAX,
                    0
                ),
            ]
        );
    }

    /// FR-3: output order is timestamp, model id, then stable request id.
    /// Same-timestamp, same-model records carry distinguishable input
    /// counts so the id tiebreak is actually proven — the record written
    /// first (a2) must sort after a1 purely by stable request id, not
    /// merely produce two equal-shaped rows.
    #[test]
    fn stable_event_order_by_timestamp_then_model_then_id() {
        let dir = temp_dir("order");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write_lines(
            &dir.join("runtime/usage"),
            "token-usage-2026-08.jsonl",
            &[
                &req(
                    "z1",
                    "s1",
                    "2026-08-10T00:00:00Z",
                    "qwen-z",
                    10,
                    1,
                    0,
                    0,
                    11,
                ),
                &req("a2", "s2", "2026-08-05T00:00:00Z", "qwen-a", 2, 1, 0, 0, 3),
                &req("a1", "s3", "2026-08-05T00:00:00Z", "qwen-a", 1, 1, 0, 0, 2),
                &req("b1", "s4", "2026-08-05T00:00:00Z", "qwen-b", 3, 1, 0, 0, 4),
                &req(
                    "z2",
                    "s5",
                    "2026-08-10T00:00:00Z",
                    "qwen-z",
                    20,
                    1,
                    0,
                    0,
                    21,
                ),
            ],
        );
        let f = usage(&cfg);
        let rows: Vec<_> = f.events.iter().map(row).collect();
        assert_eq!(
            rows,
            vec![
                // Same ts + model: a1 (input 1) precedes a2 (input 2) by
                // stable request id despite a2 appearing first in the file.
                (
                    "2026-08-05T00:00:00+00:00".into(),
                    "qwen-a".into(),
                    1,
                    1,
                    1,
                    0,
                    0
                ),
                (
                    "2026-08-05T00:00:00+00:00".into(),
                    "qwen-a".into(),
                    1,
                    2,
                    1,
                    0,
                    0
                ),
                (
                    "2026-08-05T00:00:00+00:00".into(),
                    "qwen-b".into(),
                    1,
                    3,
                    1,
                    0,
                    0
                ),
                (
                    "2026-08-10T00:00:00+00:00".into(),
                    "qwen-z".into(),
                    1,
                    10,
                    1,
                    0,
                    0
                ),
                (
                    "2026-08-10T00:00:00+00:00".into(),
                    "qwen-z".into(),
                    1,
                    20,
                    1,
                    0,
                    0
                ),
            ],
            "id tiebreak (a1 < a2) must order same-ts same-model records over file order"
        );
    }

    /// FR-4: legacy summaries respect the same [since, until) window as
    /// request records — below since and at exactly until are excluded,
    /// exactly since and just below until are included — applied after
    /// last-wins/session suppression and before event emission.
    #[test]
    fn legacy_range_filter_bounds() {
        let dir = temp_dir("legacy-range");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write(
            &dir.join("home/usage_record.jsonl"),
            &format!(
                "{}\n{}\n{}\n{}\n",
                legacy(
                    "s-below",
                    dt("2026-07-31T23:59:59.999Z").timestamp_millis(),
                    &[("qwen-max", 1, 10, 10, 0, 0, 20)],
                ),
                legacy(
                    "s-since",
                    dt("2026-08-01T00:00:00.000Z").timestamp_millis(),
                    &[("qwen-max", 1, 20, 20, 0, 0, 40)],
                ),
                legacy(
                    "s-below-until",
                    dt("2026-08-31T23:59:59.999Z").timestamp_millis(),
                    &[("qwen-max", 1, 30, 30, 0, 0, 60)],
                ),
                legacy(
                    "s-until",
                    dt("2026-09-01T00:00:00.000Z").timestamp_millis(),
                    &[("qwen-max", 1, 40, 40, 0, 0, 80)],
                ),
            ),
        );
        let f = usage(&cfg);
        let input: Vec<_> = f.events.iter().map(|e| e.input_tokens).collect();
        assert_eq!(
            f.events.len(),
            2,
            "only in-window legacy summaries emit (below since and at until excluded)"
        );
        assert_eq!(
            input,
            vec![20, 30],
            "exactly since and just below until included"
        );
        assert!(f.notes.is_empty());
    }

    /// FR-4/AC-5: a legacy session mixing one valid Qwen model, one
    /// invalid-token Qwen model, and a non-Qwen model keeps the valid
    /// event, counts exactly the invalid Qwen entry as malformed (one
    /// aggregate note, no payload/path leakage), and never treats
    /// non-Qwen models as malformed.
    #[test]
    fn legacy_mixed_session_counts_invalid_qwen_entries_only() {
        let dir = temp_dir("legacy-mixed");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        let ts = dt("2026-08-10T00:00:00Z").timestamp_millis();
        let mut m = serde_json::Map::new();
        m.insert(
            "qwen-max".into(),
            json!({
                "requests": 1, "inputTokens": 100, "outputTokens": 50,
                "cachedTokens": 0, "thoughtsTokens": 0, "totalTokens": 150
            }),
        );
        m.insert(
            "qwen-turbo".into(),
            json!({
                "requests": 1, "inputTokens": "abc", "outputTokens": 1,
                "cachedTokens": 0, "thoughtsTokens": 0, "totalTokens": 2
            }),
        );
        m.insert(
            "glm-4.6".into(),
            json!({
                "requests": 9, "inputTokens": 900, "outputTokens": 900,
                "cachedTokens": 0, "thoughtsTokens": 0, "totalTokens": 1800
            }),
        );
        write(
            &dir.join("home/usage_record.jsonl"),
            &format!(
                "{}\n",
                json!({
                    "version": 1,
                    "sessionId": "s-mixed",
                    "timestamp": ts,
                    "startTime": ts,
                    "durationMs": 1,
                    "models": m
                })
            ),
        );
        let f = usage(&cfg);
        let rows: Vec<_> = f.events.iter().map(row).collect();
        assert_eq!(
            rows,
            vec![(
                "2026-08-10T00:00:00+00:00".into(),
                "qwen-max".into(),
                1,
                100,
                50,
                0,
                0
            )],
            "valid Qwen entry survives; invalid Qwen entry and non-Qwen model emit nothing"
        );
        assert_eq!(
            f.notes,
            vec!["qwen: skipped 1 malformed local usage record(s)".to_string()],
            "exactly one aggregate note for the invalid Qwen entry"
        );
        let note = &f.notes[0];
        for leaked in [
            "abc",
            "qwen-turbo",
            "glm",
            "usage_record",
            "home",
            "runtime",
            "settings",
        ] {
            assert!(
                !note.contains(leaked),
                "note must not leak `{leaked}`: {note}"
            );
        }
    }

    /// FR-4: the legacy ledger is read only from `cfg.qwen.home` — an
    /// absent home never falls back to the runtime directory — while the
    /// runtime directory may fall back to home (FR-1).
    #[test]
    fn legacy_reads_only_from_configured_home() {
        let dir = temp_dir("legacy-home-only");
        let runtime = dir.join("runtime");
        let home = dir.join("home");
        // Only runtime_dir configured: legacy content under the runtime
        // directory must NOT be read (legacy requires cfg.qwen.home).
        let mut cfg = Config::default();
        cfg.qwen.runtime_dir = Some(runtime.clone());
        write(
            &runtime.join("usage_record.jsonl"),
            &format!(
                "{}\n",
                legacy(
                    "s-rt",
                    dt("2026-08-10T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 1, 10, 10, 0, 0, 20)]
                )
            ),
        );
        let f = usage(&cfg);
        assert!(
            f.events.is_empty(),
            "legacy must not fall back to the runtime directory"
        );
        assert!(f.notes.is_empty());

        // Runtime ledger still works with only runtime_dir configured.
        write_lines(
            &runtime.join("usage"),
            "token-usage-2026-08.jsonl",
            &[&req(
                "r1",
                "s1",
                "2026-08-10T00:00:00Z",
                "qwen-max",
                1,
                1,
                0,
                0,
                2,
            )],
        );
        let f = usage(&cfg);
        assert_eq!(
            f.events.len(),
            1,
            "runtime ledger unaffected by home-only legacy rule"
        );

        // Only home configured: runtime may fall back to home, and home
        // serves the legacy ledger.
        let mut cfg2 = Config::default();
        cfg2.qwen.home = Some(home.clone());
        write_lines(
            &home.join("usage"),
            "token-usage-2026-08.jsonl",
            &[&req(
                "r2",
                "s2",
                "2026-08-11T00:00:00Z",
                "qwen-max",
                2,
                2,
                0,
                0,
                4,
            )],
        );
        write(
            &home.join("usage_record.jsonl"),
            &format!(
                "{}\n",
                legacy(
                    "s-h",
                    dt("2026-08-12T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 3, 30, 30, 0, 0, 60)]
                )
            ),
        );
        let f = usage(&cfg2);
        let inputs: Vec<_> = f.events.iter().map(|e| e.input_tokens).collect();
        assert_eq!(
            inputs,
            vec![2, 30],
            "home serves runtime fallback and legacy ledger"
        );
    }

    /// FR-4: legacy epoch-ms + nested per-model events with the same
    /// cached-input / thinking normalization; non-Qwen models dropped.
    #[test]
    fn legacy_epoch_ms_nested_models_parse() {
        let dir = temp_dir("legacy-parse");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        let ts = dt("2026-08-10T12:34:56Z").timestamp_millis();
        write(
            &dir.join("home/usage_record.jsonl"),
            &format!(
                "{}\n",
                legacy(
                    "s-leg",
                    ts,
                    &[
                        ("qwen-max", 3, 1000, 500, 200, 100, 1400),
                        ("qwen-plus", 1, 400, 0, 400, 0, 400),
                        ("glm-4.6", 9, 900, 900, 0, 0, 1800),
                    ],
                )
            ),
        );
        let f = usage(&cfg);
        let rows: Vec<_> = f.events.iter().map(row).collect();
        assert_eq!(f.notes, Vec::<String>::new());
        assert_eq!(
            rows,
            vec![
                (
                    "2026-08-10T12:34:56+00:00".into(),
                    "qwen-max".into(),
                    3,
                    800,
                    600,
                    200,
                    0
                ),
                (
                    "2026-08-10T12:34:56+00:00".into(),
                    "qwen-plus".into(),
                    1,
                    0,
                    0,
                    400,
                    0
                ),
            ]
        );
    }

    /// FR-4: legacy rows are last-wins by session id.
    #[test]
    fn legacy_last_wins_by_session() {
        let dir = temp_dir("legacy-lastwins");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write(
            &dir.join("home/usage_record.jsonl"),
            &format!(
                "{}\n{}\n",
                legacy(
                    "s-leg",
                    dt("2026-08-10T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 3, 1000, 500, 200, 100, 1400)]
                ),
                legacy(
                    "s-leg",
                    dt("2026-08-12T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 7, 5000, 900, 1000, 300, 6200)]
                ),
            ),
        );
        let f = usage(&cfg);
        assert_eq!(
            f.events.len(),
            1,
            "repeated session keeps only the last record"
        );
        assert_eq!(f.events[0].start, dt("2026-08-12T00:00:00Z"));
        assert_eq!(f.events[0].requests, 7);
        assert_eq!(f.events[0].input_tokens, 4000, "5000 - 1000 cached");
        assert_eq!(f.events[0].output_tokens, 1200, "900 + 300 thoughts");
    }

    /// FR-4: any accepted request record for a session suppresses that
    /// session's entire legacy summary (no double counting) — including
    /// request records outside the reporting window.
    #[test]
    fn request_sessions_suppress_legacy() {
        let dir = temp_dir("precedence");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write_lines(
            &dir.join("runtime/usage"),
            "token-usage-2026-08.jsonl",
            &[
                &req(
                    "r1",
                    "s-common",
                    "2026-08-10T00:00:00Z",
                    "qwen-max",
                    10,
                    10,
                    0,
                    0,
                    20,
                ),
                // Accepted but outside [since, until): still marks the session.
                &req(
                    "r2",
                    "s-out",
                    "2026-07-20T00:00:00Z",
                    "qwen-max",
                    10,
                    10,
                    0,
                    0,
                    20,
                ),
            ],
        );
        write(
            &dir.join("home/usage_record.jsonl"),
            &format!(
                "{}\n{}\n{}\n",
                legacy(
                    "s-common",
                    dt("2026-08-11T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 99, 9900, 9900, 0, 0, 19800)]
                ),
                legacy(
                    "s-out",
                    dt("2026-08-12T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 99, 9900, 9900, 0, 0, 19800)]
                ),
                legacy(
                    "s-other",
                    dt("2026-08-13T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 4, 400, 400, 100, 100, 600)]
                ),
            ),
        );
        let f = usage(&cfg);
        let sessions_reqs: Vec<(u64, u64)> = f
            .events
            .iter()
            .map(|e| (e.requests, e.start.timestamp_millis() as u64))
            .collect();
        assert_eq!(
            sessions_reqs,
            vec![
                (1, dt("2026-08-10T00:00:00Z").timestamp_millis() as u64),
                (4, dt("2026-08-13T00:00:00Z").timestamp_millis() as u64),
            ],
            "legacy s-common and s-out suppressed; request + s-other remain"
        );
        assert_eq!(f.events.len(), 2);
    }

    /// FR-5/AC-5: malformed, future-schema, negative, non-numeric, and
    /// overflowing rows are skipped while valid rows continue; at most one
    /// aggregate skipped-record note, with no payload or path leakage.
    #[test]
    fn malformed_future_negative_non_numeric_rows_skip_with_single_note() {
        let dir = temp_dir("malformed");
        let cfg = cfg_with(&dir.join("home"), &dir.join("runtime"));
        write_lines(
            &dir.join("runtime/usage"),
            "token-usage-2026-08.jsonl",
            &[
                &req(
                    "good",
                    "s-good",
                    "2026-08-10T00:00:00Z",
                    "qwen-max",
                    5,
                    5,
                    0,
                    0,
                    10,
                ),
                "not json {{{",
                r#"{"schemaVersion":2,"id":"f1","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":0,"id":"f2","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":"1","id":"f3","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"id":"f4","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":1,"id":"f5","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":-5,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":1,"id":"f6","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1.5,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":1,"id":"f7","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":"abc","thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":1,"id":"f8","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":18446744073709551616}"#,
                r#"{"schemaVersion":1,"id":"f9","timestamp":"not-a-time","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":1,"id":"","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":1,"timestamp":"2026-08-10T00:00:00Z","sessionId":"s","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":1,"id":"f11","timestamp":"2026-08-10T00:00:00Z","sessionId":"","model":"qwen-max","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                r#"{"schemaVersion":1,"id":"f12","timestamp":"2026-08-10T00:00:00Z","sessionId":"s","inputTokens":1,"outputTokens":1,"cachedTokens":0,"thoughtsTokens":0,"totalTokens":2}"#,
                "",
            ],
        );
        write(
            &dir.join("home/usage_record.jsonl"),
            &format!(
                "{}\n{}\n{}\n{}\n",
                r#"{"version":2,"sessionId":"l1","timestamp":1786878800000,"models":{}}"#,
                r#"{"version":1,"sessionId":"l2","timestamp":-5,"models":{}}"#,
                r#"{"version":1,"sessionId":"l3","timestamp":1786878800000}"#,
                legacy(
                    "l-ok",
                    dt("2026-08-15T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 1, 10, 10, 0, 0, 20)]
                ),
            ),
        );
        let f = usage(&cfg);
        assert_eq!(
            f.events.len(),
            2,
            "one valid request + one valid legacy row"
        );
        assert_eq!(
            f.notes,
            vec!["qwen: skipped 17 malformed local usage record(s)".to_string()],
            "exactly one aggregate skipped-record note"
        );
        let note = &f.notes[0];
        for leaked in [
            "abc",
            "not-a-time",
            "-5",
            "1.5",
            "18446744073709551616",
            "not json",
            "token-usage",
            "usage_record",
            "home",
            "runtime",
            "settings",
        ] {
            assert!(
                !note.contains(leaked),
                "note must not leak `{leaked}`: {note}"
            );
        }
    }

    /// AC-5: two or more malformed files and two or more unreadable
    /// entries (cross-platform directory-at-file-path -> IsADirectory)
    /// still produce exactly one skipped-record note and one
    /// unreadable-file note for the whole call.
    #[test]
    fn multi_file_malformed_and_unreadable_aggregate_to_one_note_each() {
        let dir = temp_dir("multi-unreadable");
        let home = dir.join("home");
        let runtime = dir.join("runtime");
        let cfg = cfg_with(&home, &runtime);
        // Unreadable entries: directories at expected file paths (portable
        // IsADirectory, no permission bits).
        std::fs::create_dir_all(runtime.join("usage/token-usage-2026-08.jsonl")).unwrap();
        std::fs::create_dir_all(runtime.join("usage/token-usage-2026-09.jsonl")).unwrap();
        std::fs::create_dir_all(home.join("usage_record.jsonl")).unwrap();
        // Malformed files: two with garbage lines, one valid.
        write_lines(
            &runtime.join("usage"),
            "token-usage-2026-10.jsonl",
            &["{broken", "also broken"],
        );
        write_lines(
            &runtime.join("usage"),
            "token-usage-2026-11.jsonl",
            &["nope", "nope"],
        );
        write_lines(
            &runtime.join("usage"),
            "token-usage-2026-12.jsonl",
            &[&req(
                "ok1",
                "s1",
                "2026-08-10T00:00:00Z",
                "qwen-max",
                2,
                2,
                0,
                0,
                4,
            )],
        );
        let f = usage(&cfg);
        assert_eq!(
            f.events.len(),
            1,
            "valid row survives malformed + unreadable files"
        );
        assert_eq!(
            f.notes,
            vec![
                "qwen: skipped 4 malformed local usage record(s)".to_string(),
                "qwen: skipped 3 unreadable local usage file(s)".to_string(),
            ],
            "one aggregate note per class regardless of file count"
        );
        let dir_str = dir.display().to_string();
        for note in &f.notes {
            for leaked in [
                "token-usage",
                "usage_record",
                "home",
                "runtime",
                "settings",
                "broken",
                "nope",
                dir_str.as_str(),
            ] {
                assert!(
                    !note.contains(leaked),
                    "note must not leak `{leaked}`: {note}"
                );
            }
        }
    }

    /// AC-6: a Qwen cloud key with no local records stays configured while
    /// reporting no usage and no diagnostics; no paths means no files.
    #[test]
    fn key_only_no_files_is_configured_with_empty_usage() {
        let dir = temp_dir("key-only");
        let home = dir.join("home");
        let runtime = dir.join("runtime");
        let mut cfg = cfg_with(&home, &runtime);
        cfg.qwen.standard_key = Some("sk-std".into());
        let f = usage(&cfg);
        assert!(f.events.is_empty());
        assert!(f.notes.is_empty());
        assert!(
            Qwen.configured(&cfg),
            "a Qwen key alone marks the provider configured (FR-6/AC-6)"
        );
        let none_cfg = Config::default();
        assert!(!Qwen.configured(&none_cfg));
        assert!(usage(&none_cfg).events.is_empty());
        assert!(usage(&none_cfg).notes.is_empty());
    }

    /// FR-6: configured when any credential class or any supported local
    /// usage file exists.
    #[test]
    fn configured_matches_keys_or_local_usage_files() {
        let dir = temp_dir("configured");
        let home = dir.join("home");
        let runtime = dir.join("runtime");
        let mut cfg = cfg_with(&home, &runtime);
        assert!(!Qwen.configured(&cfg), "nothing present -> not configured");

        cfg.qwen.coding_plan_key = Some("sk-sp-coding".into());
        assert!(Qwen.configured(&cfg));
        cfg.qwen.coding_plan_key = None;
        cfg.qwen.token_plan_key = Some("sk-sp-token".into());
        assert!(Qwen.configured(&cfg));
        cfg.qwen.token_plan_key = None;

        write_lines(
            &runtime.join("usage"),
            "token-usage-2026-08.jsonl",
            &[&req(
                "c1",
                "s1",
                "2026-08-10T00:00:00Z",
                "qwen-max",
                1,
                1,
                0,
                0,
                2,
            )],
        );
        assert!(Qwen.configured(&cfg), "request file alone configures");

        std::fs::remove_dir_all(&runtime).unwrap();
        write(
            &home.join("usage_record.jsonl"),
            &format!(
                "{}\n",
                legacy(
                    "s1",
                    dt("2026-08-10T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 1, 1, 1, 0, 0, 2)]
                )
            ),
        );
        assert!(Qwen.configured(&cfg), "legacy file alone configures");
    }

    /// AC-11: usage() is read-only — runtime, legacy, and settings file
    /// metadata (length + mtime) are unchanged after collection.
    #[test]
    fn usage_never_modifies_file_metadata() {
        let dir = temp_dir("readonly");
        let home = dir.join("home");
        let runtime = dir.join("runtime");
        let cfg = cfg_with(&home, &runtime);
        write_lines(
            &runtime.join("usage"),
            "token-usage-2026-08.jsonl",
            &[&req(
                "r1",
                "s1",
                "2026-08-10T00:00:00Z",
                "qwen-max",
                1,
                1,
                0,
                0,
                2,
            )],
        );
        write(
            &home.join("usage_record.jsonl"),
            &format!(
                "{}\n",
                legacy(
                    "s2",
                    dt("2026-08-11T00:00:00Z").timestamp_millis(),
                    &[("qwen-max", 1, 1, 1, 0, 0, 2)]
                )
            ),
        );
        write(&home.join("settings.json"), "{\"env\":{}}");
        let paths = [
            runtime.join("usage/token-usage-2026-08.jsonl"),
            home.join("usage_record.jsonl"),
            home.join("settings.json"),
        ];
        let before: Vec<_> = paths
            .iter()
            .map(|p| {
                let m = std::fs::metadata(p).unwrap();
                (m.len(), m.modified().ok())
            })
            .collect();
        let f = usage(&cfg);
        assert_eq!(f.events.len(), 2);
        let after: Vec<_> = paths
            .iter()
            .map(|p| {
                let m = std::fs::metadata(p).unwrap();
                (m.len(), m.modified().ok())
            })
            .collect();
        assert_eq!(before, after, "usage() must only read, never write");
    }
}
