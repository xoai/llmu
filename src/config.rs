use anyhow::{Context, Result};
use serde::Deserialize;
use std::{collections::HashMap, fs, path::PathBuf};

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.trim().is_empty())
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// (field, source) provenance of auto-discovered credentials.
    #[serde(skip)]
    pub found: Vec<(String, String)>,
    pub anthropic: AnthropicCfg,
    pub openai: OpenAiCfg,
    pub deepseek: KeyCfg,
    pub kimi: KimiCfg,
    pub glm: GlmCfg,
    pub gemini: GeminiCfg,
    pub claude_code: ClaudeCodeCfg,
    /// Claude Pro/Max subscription (OAuth-token quota endpoint).
    pub claude: ClaudeSubCfg,
    /// OpenAI Codex CLI (ChatGPT plan) — local session logs + wham usage endpoint.
    pub codex: CodexCfg,
    /// USD per 1M tokens: model-prefix -> [input, output, cache_read, cache_write]
    pub pricing: HashMap<String, [f64; 4]>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct AnthropicCfg {
    pub admin_key: Option<String>,
}
impl AnthropicCfg {
    pub fn key(&self) -> Option<String> {
        self.admin_key
            .clone()
            .or_else(|| env("ANTHROPIC_ADMIN_KEY"))
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct OpenAiCfg {
    pub admin_key: Option<String>,
}
impl OpenAiCfg {
    pub fn key(&self) -> Option<String> {
        self.admin_key.clone().or_else(|| env("OPENAI_ADMIN_KEY"))
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct KeyCfg {
    pub api_key: Option<String>,
}
impl KeyCfg {
    pub fn key_or(&self, envs: &[&str]) -> Option<String> {
        self.api_key
            .clone()
            .or_else(|| envs.iter().find_map(|e| env(e)))
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct KimiCfg {
    pub api_key: Option<String>,
    pub base_url: Option<String>, // api.moonshot.ai | api.moonshot.cn | platform.kimi.ai key host
    /// Kimi For Coding plan credentials    env: KIMI_CODE_API_KEY
    pub code_key: Option<String>,
    pub code_base_url: Option<String>, // default https://api.kimi.com/coding/v1
}
impl KimiCfg {
    pub fn key(&self) -> Option<String> {
        self.api_key
            .clone()
            .or_else(|| env("MOONSHOT_API_KEY"))
            .or_else(|| env("KIMI_API_KEY"))
    }
    pub fn base(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| "https://api.moonshot.ai".into())
    }
    /// "Kimi For Coding" plan key (distinct from the open-platform key).
    pub fn code_key(&self) -> Option<String> {
        self.code_key.clone().or_else(|| env("KIMI_CODE_API_KEY"))
    }
    pub fn code_base(&self) -> String {
        self.code_base_url
            .clone()
            .unwrap_or_else(|| "https://api.kimi.com/coding/v1".into())
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct GlmCfg {
    pub api_key: Option<String>,
    pub base_url: Option<String>, // https://api.z.ai | https://open.bigmodel.cn
}
impl GlmCfg {
    pub fn key(&self) -> Option<String> {
        self.api_key
            .clone()
            .or_else(|| env("ZAI_API_KEY"))
            .or_else(|| env("ZHIPU_API_KEY"))
    }
    pub fn base(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| "https://api.z.ai".into())
    }
}

/// Claude Pro/Max subscription quotas via the Claude Code OAuth token.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ClaudeSubCfg {
    /// Override path to Claude Code's .credentials.json. Default search:
    /// $CLAUDE_CONFIG_DIR, ~/.claude, ~/.config/claude.
    pub credentials: Option<PathBuf>,
    /// Direct OAuth access token (auto-filled from OpenCode's auth.json
    /// when no Claude Code credentials file exists).
    pub access_token: Option<String>,
}
impl ClaudeSubCfg {
    pub fn credentials_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.credentials {
            let p = expand_tilde(p);
            return p.exists().then_some(p);
        }
        let mut cands = vec![];
        if let Some(d) = env("CLAUDE_CONFIG_DIR") {
            cands.push(PathBuf::from(d).join(".credentials.json"));
        }
        if let Some(h) = dirs::home_dir() {
            cands.push(h.join(".claude/.credentials.json"));
            cands.push(h.join(".config/claude/.credentials.json"));
        }
        cands.into_iter().find(|p| p.exists())
    }
}

/// OpenAI Codex CLI: session logs under $CODEX_HOME (default ~/.codex).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CodexCfg {
    pub enabled: bool,
    pub home: Option<PathBuf>,
}
impl Default for CodexCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            home: None,
        }
    }
}
impl CodexCfg {
    pub fn home_dir(&self) -> Option<PathBuf> {
        if let Some(h) = &self.home {
            return Some(expand_tilde(h));
        }
        if let Some(d) = env("CODEX_HOME") {
            return Some(PathBuf::from(d));
        }
        dirs::home_dir().map(|h| h.join(".codex"))
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct GeminiCfg {
    /// Optional JSONL log of per-request usageMetadata written by your app.
    pub usage_log: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClaudeCodeCfg {
    pub enabled: bool,
    pub extra_paths: Vec<PathBuf>,
}
impl Default for ClaudeCodeCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_paths: vec![],
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("llmu/config.toml")
    }

    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = path.unwrap_or_else(Self::default_path);
        let mut cfg: Config = if path.exists() {
            let raw =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Config::default()
        };
        cfg.merge_default_pricing();
        crate::discover::apply(&mut cfg);
        Ok(cfg)
    }

    /// Built-in fallback prices (USD / 1M tokens). User config overrides.
    /// These are ESTIMATES for sources without a billed-cost API — verify
    /// against each provider's pricing page and override in [pricing].
    fn merge_default_pricing(&mut self) {
        let defaults: &[(&str, [f64; 4])] = &[
            ("claude-opus", [15.0, 75.0, 1.50, 18.75]),
            ("claude-sonnet", [3.0, 15.0, 0.30, 3.75]),
            ("claude-haiku", [1.0, 5.0, 0.10, 1.25]),
            ("gpt-4o-mini", [0.15, 0.60, 0.075, 0.0]),
            ("gpt-4o", [2.50, 10.0, 1.25, 0.0]),
            ("gpt-5-mini", [0.25, 2.0, 0.025, 0.0]),
            ("deepseek-chat", [0.28, 0.42, 0.028, 0.0]),
            ("deepseek-reasoner", [0.28, 0.42, 0.028, 0.0]),
            ("glm", [0.60, 2.20, 0.11, 0.0]),
            ("kimi", [0.60, 2.50, 0.15, 0.0]),
            ("gpt-5", [1.25, 10.0, 0.125, 0.0]),
            ("deepseek", [0.27, 1.10, 0.07, 0.0]),
            ("gemini-flash", [0.30, 2.50, 0.075, 0.0]),
            ("gemini-pro", [1.25, 10.0, 0.31, 0.0]),
        ];
        for (k, v) in defaults {
            self.pricing.entry((*k).to_string()).or_insert(*v);
        }
    }

    /// Longest matching pricing key wins. A key matches when every '-'
    /// separated part appears in the model id, so "claude-sonnet" matches
    /// both "claude-sonnet-4-5" and "claude-3-5-sonnet-...".
    pub fn price_for(&self, model: &str) -> Option<[f64; 4]> {
        let m = model.to_ascii_lowercase();
        self.pricing
            .iter()
            .filter(|(k, _)| k.split('-').all(|part| m.contains(part)))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| *v)
    }

    pub fn estimate_cost(
        &self,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> Option<f64> {
        let p = self.price_for(model)?;
        Some(
            input as f64 * p[0] / 1e6
                + output as f64 * p[1] / 1e6
                + cache_read as f64 * p[2] / 1e6
                + cache_write as f64 * p[3] / 1e6,
        )
    }

    pub fn sample() -> &'static str {
        r#"# llmu configuration — OPTIONAL. Bare `llmu` auto-detects credentials from
# env vars and from Claude Code / Codex / OpenCode / kimi-cli files already
# on this machine (`llmu providers` shows what was found and from where).
# Set keys here only to override discovery. Precedence: this file > env > discovered.

[anthropic]
# Org Admin API key (sk-ant-admin-...)      env: ANTHROPIC_ADMIN_KEY
# admin_key = ""

[openai]
# Org Admin key for the Usage/Costs API     env: OPENAI_ADMIN_KEY
# admin_key = ""

[deepseek]
# env: DEEPSEEK_API_KEY
# api_key = ""

[kimi]
# env: MOONSHOT_API_KEY / KIMI_API_KEY
# api_key = ""
# base_url = "https://api.moonshot.ai"   # or https://api.moonshot.cn

[glm]
# env: ZAI_API_KEY / ZHIPU_API_KEY
# api_key = ""
# base_url = "https://api.z.ai"          # or https://open.bigmodel.cn

[gemini]
# JSONL file of per-request usageMetadata your app appends (see README).
# usage_log = "~/logs/gemini_usage.jsonl"

[claude_code]
enabled = true
# extra_paths = ["/other/home/.claude/projects"]

[claude]
# Pro/Max live quotas read Claude Code's OAuth token; auto-discovered from
# $CLAUDE_CONFIG_DIR, ~/.claude, ~/.config/claude. Override if elsewhere:
# credentials = "~/.claude/.credentials.json"

[codex]
# OpenAI Codex CLI (ChatGPT plan): tokens from $CODEX_HOME/sessions logs,
# quotas from the same logs + chatgpt.com wham/usage via ~/.codex/auth.json.
enabled = true
# home = "~/.codex"

# USD per 1M tokens: [input, output, cache_read, cache_write]
# Used only to ESTIMATE costs where no billed-cost API exists.
# Longest matching prefix wins. Keep these in sync with provider pricing pages.
[pricing]
# "claude-sonnet" = [3.0, 15.0, 0.30, 3.75]
"#
    }
}

pub fn expand_tilde(p: &std::path::Path) -> PathBuf {
    if let Ok(stripped) = p.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::default();
        c.merge_default_pricing();
        c
    }

    #[test]
    fn price_for_longest_prefix_wins() {
        let c = cfg();
        assert_eq!(
            c.price_for("gpt-4o-mini-2024-07-18"),
            Some([0.15, 0.60, 0.075, 0.0])
        );
        assert_eq!(
            c.price_for("gpt-4o-2024-08-06"),
            Some([2.50, 10.0, 1.25, 0.0])
        );
        assert_eq!(
            c.price_for("gpt-5-mini-2025-08-07"),
            Some([0.25, 2.0, 0.025, 0.0])
        );
        assert_eq!(c.price_for("gpt-5"), Some([1.25, 10.0, 0.125, 0.0]));
    }

    #[test]
    fn price_for_matches_both_claude_name_orders() {
        let c = cfg();
        assert_eq!(
            c.price_for("claude-sonnet-4-5"),
            Some([3.0, 15.0, 0.30, 3.75])
        );
        assert_eq!(
            c.price_for("claude-3-5-sonnet-20241022"),
            Some([3.0, 15.0, 0.30, 3.75])
        );
        assert_eq!(
            c.price_for("claude-opus-4-1"),
            Some([15.0, 75.0, 1.50, 18.75])
        );
        assert_eq!(
            c.price_for("claude-haiku-4-5"),
            Some([1.0, 5.0, 0.10, 1.25])
        );
    }

    #[test]
    fn price_for_kimi_and_glm_defaults() {
        let c = cfg();
        assert_eq!(
            c.price_for("kimi-k2-0711-preview"),
            Some([0.60, 2.50, 0.15, 0.0])
        );
        assert_eq!(c.price_for("glm-4.5"), Some([0.60, 2.20, 0.11, 0.0]));
        assert_eq!(c.price_for("deepseek-chat"), Some([0.28, 0.42, 0.028, 0.0]));
    }

    #[test]
    fn price_for_unknown_model_is_none() {
        assert_eq!(cfg().price_for("mystery-model-9000"), None);
    }

    #[test]
    fn estimate_cost_combines_all_token_kinds() {
        let c = cfg();
        let cost = c
            .estimate_cost("gpt-4o", 1_000_000, 1_000_000, 1_000_000, 0)
            .unwrap();
        assert!((cost - (2.50 + 10.0 + 1.25)).abs() < 1e-9);
        assert_eq!(c.estimate_cost("mystery-model-9000", 1, 1, 1, 1), None);
    }
}
