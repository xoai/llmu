use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a usage number came from. Billing APIs are authoritative;
/// local logs (e.g. Claude Code JSONL) cover subscription plans that
/// have no public billing API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Api,
    LocalLogs,
}

impl SourceKind {
    pub fn short(&self) -> &'static str {
        match self {
            SourceKind::Api => "api",
            SourceKind::LocalLogs => "local",
        }
    }
}

/// One normalized usage bucket (typically 1 hour or 1 day, per model).
#[derive(Debug, Clone, Serialize)]
pub struct UsageEvent {
    pub provider: String, // "anthropic" | "openai" | "deepseek" | "kimi" | "glm" | "gemini"
    pub source: SourceKind,
    pub model: String,
    pub start: DateTime<Utc>,
    pub requests: u64,
    /// Uncached (fresh) input tokens.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Server-side tool invocations where the provider reports them
    /// (e.g. Anthropic web search) or tool_use blocks in local logs.
    pub tool_calls: u64,
    /// Estimated cost from the pricing table (never mixed with billed cost).
    pub cost_usd: Option<f64>,
    pub cost_is_estimate: bool,
}

impl UsageEvent {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// Authoritative billed cost from a provider cost API (kept separate from
/// per-model estimates so totals are never double-counted).
#[derive(Debug, Clone, Serialize)]
pub struct BilledCost {
    pub provider: String,
    pub start: DateTime<Utc>,
    pub amount_usd: f64,
    pub description: String,
}

/// Subscription-plan quota state (e.g. 5-hour window, weekly limit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub provider: String,
    pub plan: String,
    pub window: String, // "5h" | "7d" | "month" | ...
    pub used: f64,
    pub limit: f64, // 0 = unknown limit (show used only)
    pub unit: String,
    pub resets_at: Option<DateTime<Utc>>,
}

impl QuotaSnapshot {
    pub fn pct(&self) -> f64 {
        if self.limit > 0.0 {
            (100.0 * self.used / self.limit).min(100.0)
        } else {
            0.0
        }
    }
}

/// Prepaid balance / credits (DeepSeek, Kimi, ...).
#[derive(Debug, Clone, Serialize)]
pub struct BalanceSnapshot {
    pub provider: String,
    pub currency: String,
    pub total: f64,
    pub granted: f64,
    pub topped_up: f64,
}

/// One interval between consecutive observed UTC daily closes within a
/// provider/currency balance series (FR-2.4). `from` and `to` are the
/// actual UTC dates, so missing days stay visible rather than being
/// synthesized. Decreases set `spent` only, increases `funded` only,
/// equality zeroes both; different currencies are never combined.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BalanceHistoryRow {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    pub provider: String,
    pub currency: String,
    pub opening: f64,
    pub closing: f64,
    pub spent: f64,
    pub funded: f64,
}

/// Everything one provider fetch returns.
#[derive(Debug, Default)]
pub struct Fetch {
    pub events: Vec<UsageEvent>,
    pub billed: Vec<BilledCost>,
    /// Diagnostic notes surfaced from provider cost loops (e.g. skipped
    /// quota rows); gathered and printed as-is.
    pub notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_default_notes_is_empty() {
        assert!(Fetch::default().notes.is_empty());
    }

    #[test]
    fn fetch_retains_pushed_note() {
        let mut f = Fetch::default();
        f.notes.push("quota row skipped".to_string());
        assert_eq!(f.notes, vec!["quota row skipped".to_string()]);
    }
}
