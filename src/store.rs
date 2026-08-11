use crate::types::{BalanceHistoryRow, BalanceSnapshot, QuotaSnapshot};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::io::Write;

fn data_dir() -> std::path::PathBuf {
    dirs::data_dir().unwrap_or_else(|| ".".into()).join("llmu")
}

/// DeepSeek/Kimi expose only a point-in-time balance, so we snapshot on
/// every run; day-over-day deltas are derived by [`read_balance_history`].
pub fn record_balances(balances: &[BalanceSnapshot]) -> Result<()> {
    record_balances_in(&data_dir(), balances)
}

pub(crate) fn record_balances_in(
    dir: &std::path::Path,
    balances: &[BalanceSnapshot],
) -> Result<()> {
    if balances.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("balances.jsonl"))?;
    let now = chrono::Utc::now().to_rfc3339();
    for b in balances {
        let rec = serde_json::json!({
            "ts": now,
            "provider": b.provider,
            "currency": b.currency,
            "total": b.total,
            "granted": b.granted,
            "topped_up": b.topped_up,
        });
        writeln!(f, "{rec}")?;
    }
    Ok(())
}

/// Offline daily balance history derived from the append-only snapshot
/// store (FR-2). Read-only: never writes and never requests providers.
pub struct BalanceHistory {
    pub rows: Vec<BalanceHistoryRow>,
    /// Lines skipped as malformed or non-finite; the CLI summarizes this
    /// once as a secret-free stderr note (FR-2.8).
    pub skipped: u64,
}

/// Balance history from the platform data directory's
/// `llmu/balances.jsonl` (FR-2.1). A missing file is an empty history.
pub fn read_balance_history() -> BalanceHistory {
    read_balance_history_in(&data_dir())
}

pub(crate) fn read_balance_history_in(dir: &std::path::Path) -> BalanceHistory {
    let path = dir.join("balances.jsonl");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => {
            return BalanceHistory {
                rows: vec![],
                skipped: 0,
            }
        }
    };
    // Daily close per (provider, currency, UTC date): the record with the
    // latest timestamp wins; equal timestamps pick the later physical
    // line. The BTreeMap key sorts by provider/currency/date, so input
    // order cannot affect any other result (FR-2.3).
    let mut closes: DailyCloses = BTreeMap::new();
    let mut skipped = 0u64;
    for (idx, line) in raw.lines().enumerate() {
        let Some(rec) = parse_snapshot_line(line, idx as u64) else {
            skipped += 1;
            continue;
        };
        let key = (
            rec.provider.clone(),
            rec.currency.clone(),
            rec.ts.date_naive(),
        );
        let best = closes.entry(key).or_insert((rec.ts, rec.line, rec.total));
        if rec.ts > best.0 || (rec.ts == best.0 && rec.line > best.1) {
            *best = (rec.ts, rec.line, rec.total);
        }
    }
    let mut rows = Vec::new();
    let mut current: Option<(String, String, chrono::NaiveDate, f64)> = None;
    for ((provider, currency, day), (_, _, total)) in closes {
        if let Some((prev_provider, prev_currency, prev_day, opening)) = current {
            if prev_provider == provider && prev_currency == currency {
                let (spent, funded) = if total < opening {
                    (opening - total, 0.0)
                } else if total > opening {
                    (0.0, total - opening)
                } else {
                    (0.0, 0.0)
                };
                rows.push(BalanceHistoryRow {
                    from: prev_day,
                    to: day,
                    provider: provider.clone(),
                    currency: currency.clone(),
                    opening,
                    closing: total,
                    spent,
                    funded,
                });
            }
        }
        current = Some((provider, currency, day, total));
    }
    // FR-2.6: deterministic output — `to` first, then provider, currency,
    // then `from` (BTreeMap iteration alone is provider/currency/date).
    rows.sort_by(|a, b| {
        a.to.cmp(&b.to)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.currency.cmp(&b.currency))
            .then_with(|| a.from.cmp(&b.from))
    });
    BalanceHistory { rows, skipped }
}

/// Daily close per (provider, currency, UTC date): the observed record
/// with the latest timestamp (ties broken by the later physical JSONL
/// line), which is the close for that day (FR-2.3).
type DailyCloses = BTreeMap<(String, String, chrono::NaiveDate), (DateTime<Utc>, u64, f64)>;

/// One valid stored snapshot: RFC 3339 `ts`, provider, currency, and a
/// finite numeric `total` (FR-2.2). `granted`/`topped_up` are retained
/// for compatibility but never influence derivation. Anything else —
/// missing fields, unparseable JSON, bad dates, non-finite totals — is
/// malformed and counts against the skip note.
fn parse_snapshot_line(line: &str, line_no: u64) -> Option<SnapshotLine> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;
    let ts = obj.get("ts")?.as_str()?;
    let ts = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc);
    let provider = obj.get("provider")?.as_str()?.to_string();
    let currency = obj.get("currency")?.as_str()?.to_string();
    let total = obj.get("total")?.as_f64()?;
    if !total.is_finite() {
        return None;
    }
    Some(SnapshotLine {
        ts,
        provider,
        currency,
        total,
        line: line_no,
    })
}

struct SnapshotLine {
    ts: DateTime<Utc>,
    provider: String,
    currency: String,
    total: f64,
    /// Physical JSONL line index, the tie-break for equal timestamps.
    line: u64,
}

fn quota_cache_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("quota-cache.json")
}

/// Last-known-good quota cache: quota endpoints rate-limit (Claude's
/// oauth/usage especially), so a throttled run serves the previous
/// meters, labeled with their age, instead of dropping them.
///
/// NOT thread-safe: unlocked read-modify-write on one shared JSON file.
/// Callers must serialize — `gather` collects fresh quotas on worker
/// threads and writes them here only after every thread has joined.
pub fn cache_quotas(provider: &str, quotas: &[QuotaSnapshot]) {
    cache_quotas_in(&data_dir(), provider, quotas)
}

fn cache_quotas_in(dir: &std::path::Path, provider: &str, quotas: &[QuotaSnapshot]) {
    let _ = std::fs::create_dir_all(dir);
    let path = quota_cache_path(dir);
    let mut all: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    all[provider] = serde_json::json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "quotas": quotas,
    });
    if let Ok(s) = serde_json::to_string(&all) {
        let _ = std::fs::write(&path, s);
    }
}

/// Cached quotas for a provider, plans annotated with the cache age.
pub fn cached_quotas(provider: &str) -> Option<Vec<QuotaSnapshot>> {
    cached_quotas_in(&data_dir(), provider)
}

fn cached_quotas_in(dir: &std::path::Path, provider: &str) -> Option<Vec<QuotaSnapshot>> {
    let path = quota_cache_path(dir);
    let all: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let entry = &all[provider];
    let at = entry["at"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())?
        .with_timezone(&chrono::Utc);
    let mut qs: Vec<QuotaSnapshot> = serde_json::from_value(entry["quotas"].clone()).ok()?;
    if qs.is_empty() {
        return None;
    }
    let tag = format!(" (cached {})", at.format("%H:%M"));
    for q in &mut qs {
        q.plan.push_str(&tag);
    }
    Some(qs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "llmu-store-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn quota(plan: &str) -> QuotaSnapshot {
        QuotaSnapshot {
            provider: "test".into(),
            plan: plan.into(),
            window: "5h".into(),
            used: 10.0,
            limit: 100.0,
            unit: "%".into(),
            resets_at: None,
        }
    }

    /// The cache file is a single JSON object shared by all providers; a
    /// write for one provider must merge, never replace, the others. This
    /// is the invariant the concurrent-write race in gather was violating.
    #[test]
    fn sequential_cache_writes_preserve_all_providers() {
        let dir = tmp_dir("preserve");
        cache_quotas_in(&dir, "claude", &[quota("pro")]);
        cache_quotas_in(&dir, "glm", &[quota("glm-pro")]);
        cache_quotas_in(&dir, "codex", &[quota("chatgpt")]);

        let claude = cached_quotas_in(&dir, "claude").unwrap();
        let glm = cached_quotas_in(&dir, "glm").unwrap();
        let codex = cached_quotas_in(&dir, "codex").unwrap();
        assert!(claude[0].plan.starts_with("pro (cached "));
        assert!(glm[0].plan.starts_with("glm-pro (cached "));
        assert!(codex[0].plan.starts_with("chatgpt (cached "));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewriting_a_provider_keeps_other_providers() {
        let dir = tmp_dir("rewrite");
        cache_quotas_in(&dir, "claude", &[quota("pro")]);
        cache_quotas_in(&dir, "glm", &[quota("glm-pro")]);
        cache_quotas_in(&dir, "claude", &[quota("max")]);

        let claude = cached_quotas_in(&dir, "claude").unwrap();
        assert!(claude[0].plan.starts_with("max (cached "));
        assert!(cached_quotas_in(&dir, "glm").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_empty_cache_is_none() {
        let dir = tmp_dir("empty");
        assert!(cached_quotas_in(&dir, "claude").is_none());
        cache_quotas_in(&dir, "claude", &[]);
        assert!(cached_quotas_in(&dir, "claude").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------------------------------------------------------------------
    // Offline balance history derivation (FR-2, AS-5): RED contracts.
    // -------------------------------------------------------------------

    fn hist(ts: &str, provider: &str, currency: &str, total: f64) -> String {
        serde_json::json!({
            "ts": ts,
            "provider": provider,
            "currency": currency,
            "total": total,
            "granted": 0.0,
            "topped_up": 0.0,
        })
        .to_string()
    }

    fn write_history(dir: &std::path::Path, lines: &[String]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut s = String::new();
        for l in lines {
            s.push_str(l);
            s.push('\n');
        }
        std::fs::write(dir.join("balances.jsonl"), s).unwrap();
    }

    #[test]
    fn missing_history_file_is_empty_history() {
        let dir = tmp_dir("hist-missing");
        let h = read_balance_history_in(&dir);
        assert!(h.rows.is_empty());
        assert_eq!(h.skipped, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_and_nonfinite_records_are_skipped_and_counted() {
        let dir = tmp_dir("hist-malformed");
        write_history(
            &dir,
            &[
                hist("2026-08-01T00:00:00Z", "deepseek", "USD", 10.0),
                "not json".into(),
                r#"{"ts":"bad-date","provider":"deepseek","currency":"USD","total":1.0}"#.into(),
                r#"{"provider":"deepseek","currency":"USD","total":1.0}"#.into(),
                r#"{"ts":"2026-08-02T00:00:00Z","currency":"USD","total":1.0}"#.into(),
                r#"{"ts":"2026-08-02T00:00:00Z","provider":"deepseek","total":1.0}"#.into(),
                r#"{"ts":"2026-08-02T00:00:00Z","provider":"deepseek","currency":"USD","total":"10"}"#.into(),
                r#"{"ts":"2026-08-02T00:00:00Z","provider":"deepseek","currency":"USD","total":1e999}"#.into(),
                hist("2026-08-03T00:00:00Z", "deepseek", "USD", 8.0),
            ],
        );
        let h = read_balance_history_in(&dir);
        assert_eq!(h.skipped, 7);
        assert_eq!(h.rows.len(), 1);
        assert_eq!(h.rows[0].from.to_string(), "2026-08-01");
        assert_eq!(h.rows[0].to.to_string(), "2026-08-03");
        assert_eq!(h.rows[0].closing, 8.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daily_close_is_latest_utc_timestamp_in_that_date() {
        let dir = tmp_dir("hist-close");
        write_history(
            &dir,
            &[
                hist("2026-08-01T23:59:00Z", "deepseek", "USD", 8.0),
                hist("2026-08-01T00:01:00Z", "deepseek", "USD", 10.0),
                hist("2026-08-02T00:00:30Z", "deepseek", "USD", 9.0),
                hist("2026-08-02T23:00:00Z", "deepseek", "USD", 9.5),
            ],
        );
        let h = read_balance_history_in(&dir);
        assert_eq!(h.rows.len(), 1);
        assert_eq!(h.rows[0].opening, 8.0);
        assert_eq!(h.rows[0].closing, 9.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn equal_timestamps_prefer_later_physical_line() {
        let dir = tmp_dir("hist-tie");
        write_history(
            &dir,
            &[
                hist("2026-08-01T12:00:00Z", "deepseek", "USD", 10.0),
                hist("2026-08-02T12:00:00Z", "deepseek", "USD", 7.0),
                hist("2026-08-02T12:00:00Z", "deepseek", "USD", 8.0),
                hist("2026-08-03T12:00:00Z", "deepseek", "USD", 6.0),
            ],
        );
        let h = read_balance_history_in(&dir);
        assert_eq!(h.rows.len(), 2);
        assert_eq!(h.rows[0].closing, 8.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_order_input_derives_identically_to_ordered() {
        let d1 = tmp_dir("hist-o1");
        let d2 = tmp_dir("hist-o2");
        let ordered = vec![
            hist("2026-08-01T00:00:00Z", "deepseek", "USD", 10.0),
            hist("2026-08-02T00:00:00Z", "deepseek", "USD", 9.0),
            hist("2026-08-03T00:00:00Z", "deepseek", "USD", 11.0),
        ];
        write_history(&d1, &ordered);
        write_history(
            &d2,
            &[ordered[2].clone(), ordered[0].clone(), ordered[1].clone()],
        );
        let a = read_balance_history_in(&d1);
        let b = read_balance_history_in(&d2);
        assert_eq!(a.rows, b.rows);
        assert_eq!(a.skipped, b.skipped);
        let _ = std::fs::remove_dir_all(&d1);
        let _ = std::fs::remove_dir_all(&d2);
    }

    #[test]
    fn provider_and_currency_series_never_mix() {
        let dir = tmp_dir("hist-iso");
        write_history(
            &dir,
            &[
                hist("2026-08-01T00:00:00Z", "deepseek", "USD", 10.0),
                hist("2026-08-02T00:00:00Z", "deepseek", "USD", 8.0),
                hist("2026-08-01T00:00:00Z", "kimi", "USD", 100.0),
                hist("2026-08-02T00:00:00Z", "kimi", "USD", 120.0),
                hist("2026-08-01T00:00:00Z", "deepseek", "CNY", 5.0),
                hist("2026-08-02T00:00:00Z", "deepseek", "CNY", 4.0),
            ],
        );
        let h = read_balance_history_in(&dir);
        assert_eq!(h.rows.len(), 3);
        for r in &h.rows {
            match (r.provider.as_str(), r.currency.as_str()) {
                ("deepseek", "USD") => {
                    assert_eq!(r.opening, 10.0);
                    assert_eq!(r.closing, 8.0);
                }
                ("kimi", "USD") => {
                    assert_eq!(r.opening, 100.0);
                    assert_eq!(r.closing, 120.0);
                }
                ("deepseek", "CNY") => {
                    assert_eq!(r.opening, 5.0);
                    assert_eq!(r.closing, 4.0);
                }
                other => panic!("unexpected series {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gaps_are_visible_and_never_synthesized() {
        let dir = tmp_dir("hist-gap");
        write_history(
            &dir,
            &[
                hist("2026-08-01T00:00:00Z", "deepseek", "USD", 10.0),
                hist("2026-08-02T00:00:00Z", "deepseek", "USD", 9.0),
                hist("2026-08-04T00:00:00Z", "deepseek", "USD", 8.0),
            ],
        );
        let h = read_balance_history_in(&dir);
        assert_eq!(h.rows.len(), 2);
        assert_eq!(h.rows[0].to.to_string(), "2026-08-02");
        assert_eq!(h.rows[1].from.to_string(), "2026-08-02");
        assert_eq!(h.rows[1].to.to_string(), "2026-08-04");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decrease_spends_only_increase_funds_only_equality_zeroes() {
        let dir = tmp_dir("hist-delta");
        write_history(
            &dir,
            &[
                hist("2026-08-01T00:00:00Z", "down", "USD", 10.0),
                hist("2026-08-02T00:00:00Z", "down", "USD", 7.0),
                hist("2026-08-01T00:00:00Z", "up", "USD", 5.0),
                hist("2026-08-02T00:00:00Z", "up", "USD", 9.0),
                hist("2026-08-01T00:00:00Z", "flat", "USD", 6.0),
                hist("2026-08-02T00:00:00Z", "flat", "USD", 6.0),
            ],
        );
        let h = read_balance_history_in(&dir);
        assert_eq!(h.rows.len(), 3);
        let by = |p: &str| h.rows.iter().find(|r| r.provider == p).unwrap();
        assert_eq!(by("down").spent, 3.0);
        assert_eq!(by("down").funded, 0.0);
        assert_eq!(by("up").spent, 0.0);
        assert_eq!(by("up").funded, 4.0);
        assert_eq!(by("flat").spent, 0.0);
        assert_eq!(by("flat").funded, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rows_sort_by_to_then_provider_then_currency_then_from() {
        let dir = tmp_dir("hist-order");
        write_history(
            &dir,
            &[
                hist("2026-08-01T00:00:00Z", "bravo", "USD", 1.0),
                hist("2026-08-03T00:00:00Z", "bravo", "USD", 1.5),
                hist("2026-08-01T00:00:00Z", "alpha", "USD", 2.0),
                hist("2026-08-02T00:00:00Z", "alpha", "USD", 2.5),
                hist("2026-08-01T00:00:00Z", "alpha", "CNY", 3.0),
                hist("2026-08-04T00:00:00Z", "alpha", "CNY", 3.5),
            ],
        );
        let h = read_balance_history_in(&dir);
        let keys: Vec<(String, String, String, String)> = h
            .rows
            .iter()
            .map(|r| {
                (
                    r.to.to_string(),
                    r.provider.clone(),
                    r.currency.clone(),
                    r.from.to_string(),
                )
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                (
                    "2026-08-02".into(),
                    "alpha".into(),
                    "USD".into(),
                    "2026-08-01".into()
                ),
                (
                    "2026-08-03".into(),
                    "bravo".into(),
                    "USD".into(),
                    "2026-08-01".into()
                ),
                (
                    "2026-08-04".into(),
                    "alpha".into(),
                    "CNY".into(),
                    "2026-08-01".into()
                ),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_daily_close_produces_no_rows() {
        let dir = tmp_dir("hist-single");
        write_history(
            &dir,
            &[hist("2026-08-01T00:00:00Z", "deepseek", "USD", 10.0)],
        );
        let h = read_balance_history_in(&dir);
        assert!(h.rows.is_empty());
        assert_eq!(h.skipped, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_balances_reports_write_failures() {
        let dir = tmp_dir("hist-snapfail");
        std::fs::create_dir_all(dir.join("balances.jsonl")).unwrap();
        let b = BalanceSnapshot {
            provider: "deepseek".into(),
            currency: "USD".into(),
            total: 1.0,
            granted: 0.0,
            topped_up: 0.0,
        };
        assert!(
            record_balances_in(&dir, &[b]).is_err(),
            "snapshot-write failures must be reported (FR-2.9), never discarded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
