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

/// Every provider implements the same tiny surface; unsupported
/// capabilities just return empty vectors.
pub trait Provider: Sync {
    fn id(&self) -> &'static str;
    fn configured(&self, cfg: &Config) -> bool;
    /// One-line capability summary for `llmu providers`.
    fn capabilities(&self) -> &'static str;

    fn usage(&self, _cfg: &Config, _since: DateTime<Utc>, _until: DateTime<Utc>) -> Result<Fetch> {
        Ok(Fetch::default())
    }
    fn quotas(&self, _cfg: &Config) -> Result<Vec<QuotaSnapshot>> {
        Ok(vec![])
    }
    fn balances(&self, _cfg: &Config) -> Result<Vec<BalanceSnapshot>> {
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
