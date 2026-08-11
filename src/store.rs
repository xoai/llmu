use crate::types::{BalanceSnapshot, QuotaSnapshot};
use anyhow::Result;
use std::io::Write;

fn data_dir() -> std::path::PathBuf {
    dirs::data_dir().unwrap_or_else(|| ".".into()).join("llmu")
}

/// DeepSeek/Kimi expose only a point-in-time balance, so we snapshot on
/// every run; day-over-day deltas become a derived spend series later.
pub fn record_balances(balances: &[BalanceSnapshot]) -> Result<()> {
    if balances.is_empty() {
        return Ok(());
    }
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
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
}
