pub mod anthropic;
pub mod claude_sub;
pub mod codex;
pub mod deepseek;
pub mod gemini;
pub mod glm;
pub mod kimi;
pub mod openai;

use crate::{config::Config, http, types::*};
use anyhow::Result;
use chrono::{DateTime, Utc};

/// Everything one provider quota fetch returns.
///
/// `refresh_last_known_good` is the freshness provenance that keeps the
/// last-known-good quota cache honest (AD-1, FR-3.10): `gather` rewrites
/// that cache only for nonempty snapshots observed live over the network
/// in this call. Cached-origin rows (raw TTL hits, Task 7) carry `false`
/// so old data can never be re-aged.
#[derive(Debug, Default)]
pub struct QuotaFetch {
    pub snapshots: Vec<QuotaSnapshot>,
    /// Diagnostic notes surfaced alongside quota rows (e.g. skipped
    /// meters); gathered and printed as-is.
    pub notes: Vec<String>,
    /// True only when every remote response underlying `snapshots` was
    /// observed live in this call.
    pub refresh_last_known_good: bool,
}

impl QuotaFetch {
    /// Rows observed live over the network in this call.
    pub fn live(snapshots: Vec<QuotaSnapshot>) -> Self {
        QuotaFetch {
            snapshots,
            notes: vec![],
            refresh_last_known_good: true,
        }
    }
}

/// Explicit per-fetch cache/freshness context (plan Task 7, FR-3.2):
/// threaded from the CLI through `gather` into every `Provider` method.
/// There is no hidden process-global, static, thread-local, or
/// environment freshness state; tests construct or derive it directly.
#[derive(Debug, Clone, Default)]
pub struct FetchContext {
    /// Raw HTTP TTL cache options (`[http_cache]`; zero TTL disables
    /// reads and writes, FR-3.1).
    pub cache: http::CacheOptions,
    /// Bypass raw cache reads for this fetch; successful live responses
    /// are still stored (FR-3.2).
    pub fresh: bool,
}

impl FetchContext {
    /// Build the one-shot context from configuration plus the global
    /// `--fresh` flag (FR-3.2). `dir` None resolves to the platform
    /// cache directory under `llmu/http` (FR-3.4).
    pub fn from_config(cfg: &Config, fresh: bool) -> Self {
        FetchContext {
            cache: http::CacheOptions {
                dir: None,
                ttl_seconds: cfg.http_cache.ttl_seconds,
            },
            fresh,
        }
    }
}

/// Every provider implements the same tiny surface; unsupported
/// capabilities just return empty vectors.
pub trait Provider: Sync {
    fn id(&self) -> &'static str;
    fn configured(&self, cfg: &Config) -> bool;
    /// One-line capability summary for `llmu providers`.
    fn capabilities(&self) -> &'static str;

    fn usage(
        &self,
        _cfg: &Config,
        _ctx: &FetchContext,
        _since: DateTime<Utc>,
        _until: DateTime<Utc>,
    ) -> Result<Fetch> {
        Ok(Fetch::default())
    }
    fn quotas(&self, _cfg: &Config, _ctx: &FetchContext) -> Result<QuotaFetch> {
        Ok(QuotaFetch::default())
    }
    fn balances(&self, _cfg: &Config, _ctx: &FetchContext) -> Result<Vec<BalanceSnapshot>> {
        Ok(vec![])
    }
}

pub fn all() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(anthropic::Anthropic),
        Box::new(claude_sub::ClaudeSub),
        Box::new(codex::Codex),
        Box::new(openai::OpenAi),
        Box::new(deepseek::DeepSeek),
        Box::new(kimi::Kimi),
        Box::new(glm::Glm),
        Box::new(gemini::Gemini),
    ]
}

/// Shared billed-cost pagination skeleton. `page_source` maps a page token
/// (None for the first page) to the fetched JSON; `parse_bucket` parses one
/// `data` bucket into `fetch.billed`. A 404 means the cost endpoint is
/// unavailable on some org types (silent break); every other failure pushes
/// a diagnostic note to `fetch.notes` and stops.
pub(crate) fn collect_billed_pages(
    fetch: &mut Fetch,
    provider: &str,
    mut page_source: impl FnMut(&Option<String>) -> Result<serde_json::Value>,
    mut parse_bucket: impl FnMut(&serde_json::Value, &mut Vec<BilledCost>),
) {
    let mut page: Option<String> = None;
    loop {
        let v = match page_source(&page) {
            Ok(v) => v,
            Err(e) => {
                if let Some(n) = http::cost_break_note(&e, provider) {
                    fetch.notes.push(n);
                }
                break; // cost endpoint may be unavailable on some org types
            }
        };
        for bucket in v["data"].as_array().unwrap_or(&vec![]) {
            parse_bucket(bucket, &mut fetch.billed);
        }
        page = v["next_page"].as_str().map(String::from);
        if !v["has_more"].as_bool().unwrap_or(false) || page.is_none() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::claude_sub::ClaudeSub;
    use super::codex::Codex;
    use super::glm::Glm;
    use super::kimi::Kimi;
    use super::*;

    fn snapshot(provider: &str) -> QuotaSnapshot {
        QuotaSnapshot {
            provider: provider.into(),
            plan: "pro".into(),
            window: "5h".into(),
            used: 10.0,
            limit: 100.0,
            unit: "%".into(),
            resets_at: None,
        }
    }

    /// Default/empty quota results must never refresh the last-known-good
    /// cache (AD-1 / FR-3.10: empty results set the marker false).
    #[test]
    fn default_quota_fetch_is_empty_and_never_refreshes() {
        let f = QuotaFetch::default();
        assert!(f.snapshots.is_empty());
        assert!(f.notes.is_empty());
        assert!(!f.refresh_last_known_good);
    }

    /// The live constructor marks rows observed over the network this call.
    #[test]
    fn live_quota_fetch_marks_rows_for_cache_refresh() {
        let f = QuotaFetch::live(vec![snapshot("claude")]);
        assert_eq!(f.snapshots.len(), 1);
        assert!(f.notes.is_empty());
        assert!(f.refresh_last_known_good);
    }

    /// Task 7 (RED): a fetch context derived from configuration carries
    /// the `[http_cache]` TTL and the global `--fresh` flag; `dir` stays
    /// None so production resolves the platform cache directory (FR-3.4).
    #[test]
    fn fetch_context_from_config_carries_ttl_and_freshness() {
        let mut cfg = Config::default();
        cfg.http_cache.ttl_seconds = 300;
        let ctx = FetchContext::from_config(&cfg, true);
        assert_eq!(ctx.cache.ttl_seconds, 300);
        assert!(ctx.cache.dir.is_none());
        assert!(ctx.cache.enabled());
        assert!(ctx.fresh);
        assert!(!FetchContext::from_config(&cfg, false).fresh);
        assert!(!FetchContext::default().cache.enabled());
        assert!(!FetchContext::default().fresh);
    }

    /// Task 7 (RED): every side-effect-free JSON GET in the eligible
    /// providers opts into `get_json_cached` (FR-3.3); Gemini is
    /// signature-only — its POST and local-file operations stay
    /// cache-ineligible — and the Claude OAuth token POST stays a plain
    /// `post_json`.
    #[test]
    fn eligible_provider_gets_opt_in_and_posts_stay_uncached() {
        let files: &[(&str, &str)] = &[
            ("anthropic.rs", include_str!("anthropic.rs")),
            ("claude_sub.rs", include_str!("claude_sub.rs")),
            ("codex.rs", include_str!("codex.rs")),
            ("deepseek.rs", include_str!("deepseek.rs")),
            ("glm.rs", include_str!("glm.rs")),
            ("kimi.rs", include_str!("kimi.rs")),
            ("openai.rs", include_str!("openai.rs")),
        ];
        for (name, src) in files {
            assert!(
                src.contains("get_json_cached"),
                "{name} must opt its eligible GETs into get_json_cached"
            );
        }
        let gem = include_str!("gemini.rs");
        assert!(
            !gem.contains("get_json_cached"),
            "Gemini must stay cache-ineligible (signature-only)"
        );
        assert!(
            gem.contains("post_form_json") && gem.contains("post_json"),
            "Gemini OAuth form POST and quota RPC POSTs remain uncached"
        );
        let claude = include_str!("claude_sub.rs");
        assert!(
            claude.contains("post_json"),
            "the Claude OAuth token POST must remain uncached"
        );
    }

    /// Every current override adapts: with no credentials the empty
    /// default (marker false) is returned instead of an empty vector.
    #[test]
    fn claude_quotas_without_credentials_return_empty_default() {
        let mut cfg = Config::default();
        // Block the real ~/.claude discovery so the test never reads the
        // user's credentials file or calls the network.
        cfg.claude.credentials = Some("/nonexistent/llmu-claude-test".into());
        cfg.claude.access_token = None;
        let f = ClaudeSub.quotas(&cfg, &FetchContext::default()).unwrap();
        assert!(f.snapshots.is_empty());
        assert!(!f.refresh_last_known_good);
    }

    #[test]
    fn kimi_quotas_without_credentials_return_empty_default() {
        // Key discovery falls back to KIMI_CODE_API_KEY; pin it empty so
        // the test is hermetic regardless of the developer's environment.
        std::env::set_var("KIMI_CODE_API_KEY", "");
        let f = Kimi
            .quotas(&Config::default(), &FetchContext::default())
            .unwrap();
        assert!(f.snapshots.is_empty());
        assert!(!f.refresh_last_known_good);
    }

    #[test]
    fn glm_quotas_without_credentials_return_empty_default() {
        std::env::set_var("ZAI_API_KEY", "");
        let f = Glm
            .quotas(&Config::default(), &FetchContext::default())
            .unwrap();
        assert!(f.snapshots.is_empty());
        assert!(!f.refresh_last_known_good);
    }

    /// Codex with no auth.json and no session logs keeps its existing
    /// error — the override adapts without changing failure behavior.
    #[test]
    fn codex_quotas_without_credentials_keep_existing_error() {
        let mut cfg = Config::default();
        cfg.codex.home = Some("/nonexistent/llmu-codex-test".into());
        let e = Codex
            .quotas(&cfg, &FetchContext::default())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no auth.json token and no rate_limits in session logs"),
            "existing codex quota error must survive: {e}"
        );
    }
}
