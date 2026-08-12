use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

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
    /// QwenCloud / Alibaba Model Studio (FR-1).
    pub qwen: QwenCfg,
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

/// `[qwen]` — QwenCloud / Alibaba Model Studio configuration (FR-1).
///
/// The three credential classes are NOT interchangeable: standard /
/// pay-as-you-go, Coding Plan, and Token Plan each have dedicated
/// environment and settings names, and a plan class is never inferred
/// from an `sk-sp-*` key prefix alone.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct QwenCfg {
    /// Standard / pay-as-you-go API key.
    /// Env: `DASHSCOPE_API_KEY`, then `BAILIAN_API_KEY`; settings
    /// `env.DASHSCOPE_API_KEY`, then `env.BAILIAN_API_KEY` (FR-1).
    pub standard_key: Option<String>,
    /// Qwen Coding Plan API key. Only
    /// `BAILIAN_CODING_PLAN_API_KEY` / settings
    /// `env.BAILIAN_CODING_PLAN_API_KEY` — never a standard or Token
    /// Plan source (FR-1).
    pub coding_plan_key: Option<String>,
    /// Qwen Token Plan API key. Only
    /// `BAILIAN_TOKEN_PLAN_API_KEY` / settings
    /// `env.BAILIAN_TOKEN_PLAN_API_KEY` — never a standard or Coding
    /// Plan source (FR-1).
    pub token_plan_key: Option<String>,
    /// Qwen Code settings/home directory. Precedence: this override,
    /// `QWEN_HOME`, `~/.qwen` (FR-1).
    pub home: Option<PathBuf>,
    /// Qwen Code runtime/output directory. Precedence: this override,
    /// `QWEN_RUNTIME_DIR`, settings `advanced.runtimeOutputDir`, the
    /// effective Qwen home (FR-1).
    pub runtime_dir: Option<PathBuf>,
}

/// Typed values of Qwen Code's `${home}/settings.json` (FR-2). Unknown
/// fields are ignored; a wrong-typed `env` / `advanced` fails the whole
/// parse so discovery can skip the file untouched (AC-10).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct QwenSettings {
    /// The settings `env` block — exact credential key names only (FR-2).
    pub env: HashMap<String, String>,
    /// The settings `advanced` block.
    pub advanced: QwenAdvanced,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct QwenAdvanced {
    /// `advanced.runtimeOutputDir` — a relative value is anchored under
    /// the effective Qwen home, never the process working directory
    /// (FR-2).
    #[serde(rename = "runtimeOutputDir")]
    pub runtime_output_dir: Option<String>,
}

/// Injected environment lookup: returns `Some` only for set, non-empty
/// values. Tests supply sandboxed lookups so no test reads the real
/// process environment (hermetic discovery).
pub(crate) type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Resolve one Qwen credential class: explicit config, then each
/// dedicated environment name, then the settings `env` block. Returns the
/// value plus its provenance source string.
fn qwen_key(
    cfg: Option<&str>,
    env_names: &[&str],
    settings_names: &[&str],
    env: EnvLookup,
    settings: &QwenSettings,
    settings_src: &str,
) -> Option<(String, String)> {
    if let Some(v) = cfg {
        return Some((v.to_string(), "config".into()));
    }
    for name in env_names {
        if let Some(v) = env(name) {
            return Some((v, format!("env {name}")));
        }
    }
    for name in settings_names {
        if let Some(v) = settings.env.get(*name).filter(|v| !v.trim().is_empty()) {
            return Some((v.clone(), format!("settings {settings_src}")));
        }
    }
    None
}

/// Standard key: `DASHSCOPE_API_KEY`, then `BAILIAN_API_KEY` — env and
/// settings in that order (FR-1). A value here is standard regardless of
/// its prefix; plan classes are never inferred from `sk-sp-*` (AC-1).
pub(crate) fn qwen_standard_key(
    cfg: Option<&str>,
    env: EnvLookup,
    settings: &QwenSettings,
    settings_src: &str,
) -> Option<(String, String)> {
    qwen_key(
        cfg,
        &["DASHSCOPE_API_KEY", "BAILIAN_API_KEY"],
        &["DASHSCOPE_API_KEY", "BAILIAN_API_KEY"],
        env,
        settings,
        settings_src,
    )
}

/// Coding Plan key: `BAILIAN_CODING_PLAN_API_KEY` only (FR-1, AC-1).
pub(crate) fn qwen_coding_plan_key(
    cfg: Option<&str>,
    env: EnvLookup,
    settings: &QwenSettings,
    settings_src: &str,
) -> Option<(String, String)> {
    qwen_key(
        cfg,
        &["BAILIAN_CODING_PLAN_API_KEY"],
        &["BAILIAN_CODING_PLAN_API_KEY"],
        env,
        settings,
        settings_src,
    )
}

/// Token Plan key: `BAILIAN_TOKEN_PLAN_API_KEY` only (FR-1, AC-1).
pub(crate) fn qwen_token_plan_key(
    cfg: Option<&str>,
    env: EnvLookup,
    settings: &QwenSettings,
    settings_src: &str,
) -> Option<(String, String)> {
    qwen_key(
        cfg,
        &["BAILIAN_TOKEN_PLAN_API_KEY"],
        &["BAILIAN_TOKEN_PLAN_API_KEY"],
        env,
        settings,
        settings_src,
    )
}

/// Effective Qwen home: explicit config, `QWEN_HOME`, then the default
/// `~/.qwen` (FR-1). The default is injected so tests never consult a
/// real home directory (hermetic discovery).
pub(crate) fn qwen_home(
    cfg: Option<&Path>,
    env: EnvLookup,
    default_home: Option<&Path>,
) -> Option<(PathBuf, String)> {
    if let Some(p) = cfg {
        return Some((expand_tilde(p), "config".into()));
    }
    if let Some(d) = env("QWEN_HOME") {
        return Some((PathBuf::from(d), "env QWEN_HOME".into()));
    }
    default_home.map(|p| (p.to_path_buf(), "default ~/.qwen".into()))
}

/// Effective Qwen runtime directory: explicit config, `QWEN_RUNTIME_DIR`,
/// settings `advanced.runtimeOutputDir`, then the effective Qwen home
/// (FR-1). A relative settings output is anchored under the Qwen home —
/// never the process working directory (FR-2).
pub(crate) fn qwen_runtime(
    cfg: Option<&Path>,
    env: EnvLookup,
    settings_runtime: Option<&str>,
    settings_src: &str,
    home: &Path,
) -> Option<(PathBuf, String)> {
    if let Some(p) = cfg {
        return Some((expand_tilde(p), "config".into()));
    }
    if let Some(d) = env("QWEN_RUNTIME_DIR") {
        return Some((PathBuf::from(d), "env QWEN_RUNTIME_DIR".into()));
    }
    if let Some(r) = settings_runtime.filter(|r| !r.trim().is_empty()) {
        let p = PathBuf::from(r);
        let p = if p.is_absolute() { p } else { home.join(p) };
        return Some((
            p,
            format!("settings {settings_src} advanced.runtimeOutputDir"),
        ));
    }
    Some((home.to_path_buf(), "qwen home".into()))
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
# Discovery is read-only except a validated OAuth refresh of a supported
# plaintext Gemini CLI / Claude Code credential file (see docs/providers.md);
# encrypted stores, keychains, and access-only tokens are never touched.

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
#
# Code Assist quotas auto-discover Gemini CLI's plaintext OAuth credentials
# (${GEMINI_CLI_HOME:-$HOME}/.gemini/oauth_creds.json). Override the file or
# the cloud project here; refresh is the ONLY credential write llmu performs,
# and encrypted/keychain stores (sibling gemini-credentials.json) are never
# read or modified.
# credentials = "~/.gemini/oauth_creds.json"
# project = "my-cloud-project"  # else GOOGLE_CLOUD_PROJECT, then GOOGLE_CLOUD_PROJECT_ID

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

[qwen]
# Qwen (Alibaba Cloud Model Studio / QwenCloud): usage from local Qwen Code
# records only — no Qwen network call, no built-in price guesses, and no
# console API. Three NON-interchangeable key classes; an sk-sp-* prefix
# never identifies the plan class.
# env: standard DASHSCOPE_API_KEY then BAILIAN_API_KEY; Coding Plan
#      BAILIAN_CODING_PLAN_API_KEY only; Token Plan BAILIAN_TOKEN_PLAN_API_KEY only.
# standard_key = ""
# coding_plan_key = ""
# token_plan_key = ""
# Qwen home precedence: [qwen].home > QWEN_HOME > ~/.qwen
# home = "~/.qwen"
# Runtime precedence: [qwen].runtime_dir > QWEN_RUNTIME_DIR > settings
# advanced.runtimeOutputDir (relative -> under the Qwen home) > Qwen home.
# runtime_dir = ""

[http_cache]
# Optional TTL (seconds) for caching successful JSON responses of eligible
# side-effect-free GET requests only — never OAuth exchanges, POSTs, or
# local-file reads. Zero (default) disables cache reads and writes entirely.
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
        let parsed: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(parsed.http_cache.ttl_seconds, 0);
    }

    #[test]
    fn http_cache_parses_positive_ttl() {
        let c: Config = toml::from_str("[http_cache]\nttl_seconds = 300\n").unwrap();
        assert_eq!(c.http_cache.ttl_seconds, 300);
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

    // -------------------------------------------------------------------
    // Qwen Cloud credential classes and path precedence (FR-1, AC-1, AC-2)
    // -------------------------------------------------------------------

    const QWEN_SETTINGS_SRC: &str = "/tmp/llmu-qwen/settings.json";

    fn no_env(_k: &str) -> Option<String> {
        None
    }

    fn envmap<'a>(pairs: &'a [(&'a str, &'a str)]) -> std::collections::HashMap<&'a str, &'a str> {
        pairs.iter().copied().collect()
    }

    fn qwen_settings(pairs: &[(&str, &str)]) -> QwenSettings {
        QwenSettings {
            env: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            advanced: QwenAdvanced::default(),
        }
    }

    fn runtime_settings(runtime: Option<&str>) -> QwenSettings {
        QwenSettings {
            env: std::collections::HashMap::new(),
            advanced: QwenAdvanced {
                runtime_output_dir: runtime.map(String::from),
            },
        }
    }

    #[test]
    fn qwen_standard_key_prefers_config_then_env_then_settings() {
        let envs = envmap(&[
            ("DASHSCOPE_API_KEY", "env-dash"),
            ("BAILIAN_API_KEY", "env-bailian"),
        ]);
        let env = |k: &str| envs.get(k).map(|v| v.to_string());
        let settings = qwen_settings(&[
            ("DASHSCOPE_API_KEY", "set-dash"),
            ("BAILIAN_API_KEY", "set-bailian"),
        ]);

        let (v, src) =
            qwen_standard_key(Some("cfg-key"), &env, &settings, QWEN_SETTINGS_SRC).unwrap();
        assert_eq!(v, "cfg-key");
        assert_eq!(
            src, "config",
            "explicit config precedes env and settings (FR-1)"
        );

        let (v, src) = qwen_standard_key(None, &env, &settings, QWEN_SETTINGS_SRC).unwrap();
        assert_eq!(v, "env-dash");
        assert_eq!(
            src, "env DASHSCOPE_API_KEY",
            "DASHSCOPE_API_KEY precedes BAILIAN_API_KEY"
        );

        let (v, src) = qwen_standard_key(None, &no_env, &settings, QWEN_SETTINGS_SRC).unwrap();
        assert_eq!(v, "set-dash");
        assert_eq!(
            src, "settings /tmp/llmu-qwen/settings.json",
            "settings env block is the last fallback"
        );
    }

    #[test]
    fn qwen_standard_key_accepts_bailian_as_second_env_fallback() {
        let envs = envmap(&[("BAILIAN_API_KEY", "env-bailian")]);
        let env = |k: &str| envs.get(k).map(|v| v.to_string());
        let (v, src) =
            qwen_standard_key(None, &env, &qwen_settings(&[]), QWEN_SETTINGS_SRC).unwrap();
        assert_eq!(v, "env-bailian");
        assert_eq!(src, "env BAILIAN_API_KEY");

        let settings = qwen_settings(&[("BAILIAN_API_KEY", "set-bailian")]);
        let (v, src) = qwen_standard_key(None, &no_env, &settings, QWEN_SETTINGS_SRC).unwrap();
        assert_eq!(v, "set-bailian");
        assert_eq!(src, "settings /tmp/llmu-qwen/settings.json");
    }

    #[test]
    fn qwen_standard_key_skips_empty_settings_values() {
        let settings = qwen_settings(&[
            ("DASHSCOPE_API_KEY", ""),
            ("BAILIAN_API_KEY", "set-bailian"),
        ]);
        let (v, src) = qwen_standard_key(None, &no_env, &settings, QWEN_SETTINGS_SRC).unwrap();
        assert_eq!(v, "set-bailian");
        assert_eq!(src, "settings /tmp/llmu-qwen/settings.json");
    }

    #[test]
    fn qwen_ac1_given_only_coding_plan_env_token_plan_stays_unset() {
        let envs = envmap(&[("BAILIAN_CODING_PLAN_API_KEY", "sk-sp-X")]);
        let env = |k: &str| envs.get(k).map(|v| v.to_string());
        let settings = qwen_settings(&[]);
        assert_eq!(
            qwen_coding_plan_key(None, &env, &settings, QWEN_SETTINGS_SRC)
                .unwrap()
                .0,
            "sk-sp-X"
        );
        assert!(
            qwen_token_plan_key(None, &env, &settings, QWEN_SETTINGS_SRC).is_none(),
            "Token Plan must not read BAILIAN_CODING_PLAN_API_KEY (AC-1)"
        );
        assert!(
            qwen_standard_key(None, &env, &settings, QWEN_SETTINGS_SRC).is_none(),
            "standard must not read the Coding Plan variable (AC-1)"
        );
    }

    #[test]
    fn qwen_ac1_given_only_token_plan_env_coding_plan_stays_unset() {
        let envs = envmap(&[("BAILIAN_TOKEN_PLAN_API_KEY", "sk-sp-Y")]);
        let env = |k: &str| envs.get(k).map(|v| v.to_string());
        let settings = qwen_settings(&[]);
        assert_eq!(
            qwen_token_plan_key(None, &env, &settings, QWEN_SETTINGS_SRC)
                .unwrap()
                .0,
            "sk-sp-Y"
        );
        assert!(
            qwen_coding_plan_key(None, &env, &settings, QWEN_SETTINGS_SRC).is_none(),
            "Coding Plan must not read BAILIAN_TOKEN_PLAN_API_KEY (AC-1)"
        );
        assert!(
            qwen_standard_key(None, &env, &settings, QWEN_SETTINGS_SRC).is_none(),
            "standard must not read the Token Plan variable (AC-1)"
        );
    }

    #[test]
    fn qwen_ac1_bare_sk_sp_under_dashscope_populates_standard_only() {
        let envs = envmap(&[("DASHSCOPE_API_KEY", "sk-sp-bare")]);
        let env = |k: &str| envs.get(k).map(|v| v.to_string());
        let settings = qwen_settings(&[]);
        let (v, src) = qwen_standard_key(None, &env, &settings, QWEN_SETTINGS_SRC).unwrap();
        assert_eq!(
            v, "sk-sp-bare",
            "a bare sk-sp-* value stays standard (AC-1)"
        );
        assert_eq!(src, "env DASHSCOPE_API_KEY");
        assert!(
            qwen_coding_plan_key(None, &env, &settings, QWEN_SETTINGS_SRC).is_none(),
            "never promote an sk-sp-* standard value to Coding Plan (AC-1)"
        );
        assert!(
            qwen_token_plan_key(None, &env, &settings, QWEN_SETTINGS_SRC).is_none(),
            "never promote an sk-sp-* standard value to Token Plan (AC-1)"
        );
    }

    #[test]
    fn qwen_plan_keys_never_fall_back_to_standard_sources() {
        let envs = envmap(&[("DASHSCOPE_API_KEY", "sk-dash")]);
        let env = |k: &str| envs.get(k).map(|v| v.to_string());
        let settings = qwen_settings(&[("DASHSCOPE_API_KEY", "set-dash")]);
        assert!(
            qwen_coding_plan_key(None, &env, &settings, QWEN_SETTINGS_SRC).is_none(),
            "Coding Plan recognizes only its dedicated env/settings name (FR-1)"
        );
        assert!(
            qwen_token_plan_key(None, &env, &settings, QWEN_SETTINGS_SRC).is_none(),
            "Token Plan recognizes only its dedicated env/settings name (FR-1)"
        );
    }

    #[test]
    fn qwen_home_prefers_config_then_env_then_default() {
        let default = Path::new("/tmp/llmu-default/.qwen");
        let envs = envmap(&[("QWEN_HOME", "/tmp/llmu-env-home")]);
        let env = |k: &str| envs.get(k).map(|v| v.to_string());

        let (p, src) =
            qwen_home(Some(Path::new("/tmp/llmu-cfg-home")), &env, Some(default)).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/llmu-cfg-home"));
        assert_eq!(src, "config", "explicit home precedes QWEN_HOME (FR-1)");

        let (p, src) = qwen_home(None, &env, Some(default)).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/llmu-env-home"));
        assert_eq!(src, "env QWEN_HOME", "QWEN_HOME precedes ~/.qwen (FR-1)");

        let (p, src) = qwen_home(None, &no_env, Some(default)).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/llmu-default/.qwen"));
        assert_eq!(src, "default ~/.qwen");

        assert!(
            qwen_home(None, &no_env, None).is_none(),
            "no home, no env, no default -> no Qwen home"
        );
    }

    #[test]
    fn qwen_runtime_prefers_config_then_env_then_settings_then_home() {
        let home = Path::new("/tmp/llmu-qwen-home");
        let envs = envmap(&[("QWEN_RUNTIME_DIR", "/tmp/llmu-env-runtime")]);
        let env = |k: &str| envs.get(k).map(|v| v.to_string());

        let (p, src) = qwen_runtime(
            Some(Path::new("/tmp/llmu-cfg-runtime")),
            &env,
            runtime_settings(Some("rel/runtime"))
                .advanced
                .runtime_output_dir
                .as_deref(),
            QWEN_SETTINGS_SRC,
            home,
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/tmp/llmu-cfg-runtime"));
        assert_eq!(src, "config", "explicit runtime precedes env (FR-1)");

        let (p, src) = qwen_runtime(None, &env, None, QWEN_SETTINGS_SRC, home).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/llmu-env-runtime"));
        assert_eq!(
            src, "env QWEN_RUNTIME_DIR",
            "QWEN_RUNTIME_DIR precedes settings (FR-1)"
        );

        let (p, src) = qwen_runtime(
            None,
            &no_env,
            runtime_settings(Some("/abs/runtime"))
                .advanced
                .runtime_output_dir
                .as_deref(),
            QWEN_SETTINGS_SRC,
            home,
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/abs/runtime"));
        assert_eq!(
            src, "settings /tmp/llmu-qwen/settings.json advanced.runtimeOutputDir",
            "settings runtimeOutputDir precedes the home fallback (FR-1)"
        );

        let (p, src) = qwen_runtime(None, &no_env, None, QWEN_SETTINGS_SRC, home).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/llmu-qwen-home"));
        assert_eq!(
            src, "qwen home",
            "effective Qwen home is the last runtime fallback (FR-1)"
        );
    }

    #[test]
    fn qwen_runtime_relative_output_anchors_under_effective_home() {
        let home = Path::new("/tmp/llmu-qwen-home");
        let (p, _) = qwen_runtime(
            None,
            &no_env,
            runtime_settings(Some("runtime/out"))
                .advanced
                .runtime_output_dir
                .as_deref(),
            QWEN_SETTINGS_SRC,
            home,
        )
        .unwrap();
        assert_eq!(
            p,
            home.join("runtime/out"),
            "relative runtimeOutputDir resolves under the effective Qwen home, never CWD (FR-2)"
        );
        let (p, _) = qwen_runtime(
            None,
            &no_env,
            runtime_settings(Some("usage"))
                .advanced
                .runtime_output_dir
                .as_deref(),
            QWEN_SETTINGS_SRC,
            home,
        )
        .unwrap();
        assert_eq!(p, home.join("usage"));
        let (p, _) = qwen_runtime(
            None,
            &no_env,
            runtime_settings(Some("/abs/out"))
                .advanced
                .runtime_output_dir
                .as_deref(),
            QWEN_SETTINGS_SRC,
            home,
        )
        .unwrap();
        assert_eq!(
            p,
            PathBuf::from("/abs/out"),
            "absolute runtimeOutputDir is used as-is"
        );
        assert!(
            qwen_runtime(
                None,
                &no_env,
                runtime_settings(Some("  "))
                    .advanced
                    .runtime_output_dir
                    .as_deref(),
                QWEN_SETTINGS_SRC,
                home,
            )
            .is_some(),
            "whitespace-only runtime falls through to the home fallback"
        );
        let (p, _) = qwen_runtime(
            None,
            &no_env,
            runtime_settings(Some("  "))
                .advanced
                .runtime_output_dir
                .as_deref(),
            QWEN_SETTINGS_SRC,
            home,
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/tmp/llmu-qwen-home"));
    }

    #[test]
    fn qwen_settings_parse_exact_env_and_runtime_output_dir() {
        let s: QwenSettings = serde_json::from_str(
            r#"{"env":{"DASHSCOPE_API_KEY":"sk-a","BAILIAN_CODING_PLAN_API_KEY":"sk-b"},"advanced":{"runtimeOutputDir":"runtime"},"other":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(
            s.env.get("DASHSCOPE_API_KEY").map(String::as_str),
            Some("sk-a")
        );
        assert_eq!(
            s.env.get("BAILIAN_CODING_PLAN_API_KEY").map(String::as_str),
            Some("sk-b")
        );
        assert_eq!(s.advanced.runtime_output_dir.as_deref(), Some("runtime"));
    }

    #[test]
    fn qwen_settings_missing_blocks_default_cleanly() {
        let s: QwenSettings = serde_json::from_str(r#"{}"#).unwrap();
        assert!(s.env.is_empty());
        assert!(s.advanced.runtime_output_dir.is_none());
        let s: QwenSettings = serde_json::from_str(r#"{"env":{}}"#).unwrap();
        assert!(s.env.is_empty());
    }

    #[test]
    fn qwen_settings_wrong_typed_values_fail_parse() {
        assert!(serde_json::from_str::<QwenSettings>(r#"{"env":"oops"}"#).is_err());
        assert!(
            serde_json::from_str::<QwenSettings>(r#"{"env":{"DASHSCOPE_API_KEY":42}}"#).is_err()
        );
        assert!(
            serde_json::from_str::<QwenSettings>(r#"{"advanced":{"runtimeOutputDir":3}}"#).is_err()
        );
    }
}
