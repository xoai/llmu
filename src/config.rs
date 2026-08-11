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
    /// Optional HTTP response TTL cache (FR-3). Zero TTL disables it.
    pub http_cache: HttpCacheCfg,
    /// USD per 1M tokens: model-prefix -> [input, output, cache_read, cache_write]
    pub pricing: HashMap<String, [f64; 4]>,
}

/// `[http_cache]` — optional raw GET/JSON response caching. `ttl_seconds`
/// zero (the default) disables cache reads and writes entirely, so existing
/// configs and behavior are unchanged (FR-3.1, FR-7.5).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct HttpCacheCfg {
    /// Entry lifetime in seconds; 0 disables caching (default).
    pub ttl_seconds: u64,
}
impl HttpCacheCfg {
    pub fn enabled(&self) -> bool {
        self.ttl_seconds > 0
    }
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

/// Gemini Code Assist: optional usage JSONL plus optional Code Assist
/// credential and project overrides (FR-5.1). Quota discovery reads Gemini
/// CLI's plaintext `oauth_creds.json`; an encrypted sibling marker is
/// diagnosed, never mutated (FR-5.2).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct GeminiCfg {
    /// Optional JSONL log of per-request usageMetadata written by your app.
    pub usage_log: Option<PathBuf>,
    /// Override path to Gemini CLI's plaintext OAuth credentials
    /// (`oauth_creds.json`). Default:
    /// `${GEMINI_CLI_HOME:-$HOME}/.gemini/oauth_creds.json` (FR-5.1).
    pub credentials: Option<PathBuf>,
    /// Code Assist cloud project override. Precedence: config >
    /// `GOOGLE_CLOUD_PROJECT` > `GOOGLE_CLOUD_PROJECT_ID`; a project
    /// returned by `loadCodeAssist` is authoritative (FR-5.5).
    pub project: Option<String>,
}
impl GeminiCfg {
    /// Credential store directory (FR-5.1): the explicit override's parent,
    /// else `${GEMINI_CLI_HOME:-$HOME}/.gemini`.
    fn credentials_dir(&self) -> Option<PathBuf> {
        if let Some(p) = &self.credentials {
            return expand_tilde(p).parent().map(PathBuf::from);
        }
        let home = env("GEMINI_CLI_HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)?;
        Some(home.join(".gemini"))
    }

    /// Supported plaintext credential path when it exists (FR-5.2): an
    /// explicit override, else the default `oauth_creds.json`. The
    /// existence check makes discovery hermetic and keeps an explicit
    /// override from silently falling back to `$HOME`.
    pub fn credentials_path(&self) -> Option<PathBuf> {
        let p = match &self.credentials {
            Some(p) => expand_tilde(p),
            None => self.credentials_dir()?.join("oauth_creds.json"),
        };
        p.exists().then_some(p)
    }

    /// Sibling encrypted-store marker (FR-5.2): Gemini CLI `v0.39.1`
    /// writes keychain-fallback credentials encrypted to
    /// `gemini-credentials.json` next to `oauth_creds.json`. Presence with
    /// no plaintext file means encrypted/keychain storage is unsupported
    /// in this release; llmu never reads or mutates that store.
    pub fn encrypted_marker_path(&self) -> Option<PathBuf> {
        let p = match &self.credentials {
            Some(p) => expand_tilde(p).with_file_name("gemini-credentials.json"),
            None => self.credentials_dir()?.join("gemini-credentials.json"),
        };
        p.exists().then_some(p)
    }

    /// Effective project (FR-5.5): config, then `GOOGLE_CLOUD_PROJECT`,
    /// then `GOOGLE_CLOUD_PROJECT_ID`. A `loadCodeAssist`-returned
    /// project is authoritative and resolved in the provider, not here.
    pub fn project_override(&self) -> Option<String> {
        self.project
            .clone()
            .or_else(|| env("GOOGLE_CLOUD_PROJECT"))
            .or_else(|| env("GOOGLE_CLOUD_PROJECT_ID"))
    }
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

[http_cache]
# Optional TTL (seconds) for raw HTTP GET/JSON response caching.
# Zero (default) disables cache reads and writes entirely.
# ttl_seconds = 300

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

    #[test]
    fn http_cache_defaults_to_disabled() {
        let c = cfg();
        assert_eq!(c.http_cache.ttl_seconds, 0);
        assert!(!c.http_cache.enabled());
        let parsed: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(parsed.http_cache.ttl_seconds, 0);
        assert!(!parsed.http_cache.enabled());
    }

    #[test]
    fn http_cache_parses_positive_ttl() {
        let c: Config = toml::from_str("[http_cache]\nttl_seconds = 300\n").unwrap();
        assert_eq!(c.http_cache.ttl_seconds, 300);
        assert!(c.http_cache.enabled());
    }

    #[test]
    fn http_cache_section_appears_in_sample() {
        assert!(Config::sample().contains("[http_cache]"));
        assert!(Config::sample().contains("ttl_seconds"));
    }

    // -------------------------------------------------------------------
    // Gemini Code Assist credential/project discovery (FR-5.1, FR-5.2, FR-5.5)
    // -------------------------------------------------------------------

    #[test]
    fn gemini_default_credential_path_honors_gemini_cli_home() {
        let dir = std::env::temp_dir().join(format!(
            "llmu-gemini-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".gemini")).unwrap();
        std::fs::write(dir.join(".gemini/oauth_creds.json"), b"{}").unwrap();
        std::env::set_var("GEMINI_CLI_HOME", &dir);
        let c = Config::default();
        let got = c
            .gemini
            .credentials_path()
            .expect("default plaintext path exists");
        assert_eq!(
            got,
            dir.join(".gemini/oauth_creds.json"),
            "default credential path is ${{GEMINI_CLI_HOME:-$HOME}}/.gemini/oauth_creds.json (FR-5.1)"
        );
        std::env::remove_var("GEMINI_CLI_HOME");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gemini_credentials_and_marker_resolve_next_to_override() {
        let dir = std::env::temp_dir().join(format!(
            "llmu-gemini-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Only the encrypted sibling marker exists — no plaintext file.
        std::fs::write(dir.join("gemini-credentials.json"), b"iv:tag:enc").unwrap();
        let c = Config {
            gemini: GeminiCfg {
                credentials: Some(dir.join("oauth_creds.json")),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(
            c.gemini.credentials_path().is_none(),
            "an absent plaintext override must not claim supported credentials (FR-5.2)"
        );
        assert_eq!(
            c.gemini.encrypted_marker_path(),
            Some(dir.join("gemini-credentials.json")),
            "the encrypted-store marker is the sibling gemini-credentials.json (FR-5.2)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gemini_project_precedence_config_env_then_env_id() {
        let mut c = Config::default();
        c.gemini.project = Some("cfg-proj".into());
        std::env::set_var("GOOGLE_CLOUD_PROJECT", "env-proj");
        std::env::set_var("GOOGLE_CLOUD_PROJECT_ID", "id-proj");
        assert_eq!(
            c.gemini.project_override().as_deref(),
            Some("cfg-proj"),
            "config beats env (FR-5.5)"
        );
        c.gemini.project = None;
        assert_eq!(
            c.gemini.project_override().as_deref(),
            Some("env-proj"),
            "GOOGLE_CLOUD_PROJECT beats GOOGLE_CLOUD_PROJECT_ID (FR-5.5)"
        );
        std::env::remove_var("GOOGLE_CLOUD_PROJECT");
        assert_eq!(
            c.gemini.project_override().as_deref(),
            Some("id-proj"),
            "GOOGLE_CLOUD_PROJECT_ID is the last fallback (FR-5.5)"
        );
        std::env::remove_var("GOOGLE_CLOUD_PROJECT_ID");
        assert!(c.gemini.project_override().is_none());
    }
}
