//! Zero-config credential discovery.
//!
//! Most of what llmu needs already exists on the machine, written by the
//! CLIs people use. On every run, unset config fields are filled from
//! (in order): explicit config.toml > env vars > credential files of
//! other tools. Everything is READ-ONLY and best-effort; provenance is
//! recorded so `llmu providers` can show where each credential came from.
//!
//! Sources probed:
//! - env: ANTHROPIC_ADMIN_KEY / ANTHROPIC_API_KEY (only if sk-ant-admin…),
//!   OPENAI_ADMIN_KEY / OPENAI_API_KEY (only if sk-admin…), DEEPSEEK_API_KEY,
//!   MOONSHOT_API_KEY, KIMI_API_KEY, KIMI_CODE_API_KEY, ZAI_API_KEY,
//!   ZHIPU_API_KEY, and the ANTHROPIC_AUTH_TOKEN + ANTHROPIC_BASE_URL pair
//!   (routes to GLM / Kimi Code / DeepSeek by base host).
//! - Claude Code: ~/.claude/settings.json `env` block (the standard way
//!   Z.ai / Kimi coding plans are wired into Claude Code), plus
//!   .credentials.json for Pro/Max OAuth (handled in config.rs).
//! - Codex: $CODEX_HOME auth.json + session logs (handled in the provider).
//! - OpenCode: {XDG_DATA_HOME|~/.local/share}/opencode/auth.json —
//!   provider-keyed entries {"type":"api","key":…} or {"type":"oauth",
//!   "access":…}.
//! - kimi-cli: ~/.kimi/credentials/*.json (OAuth access_token for the
//!   Kimi For Coding platform).

use crate::config::Config;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn env(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// Map an Anthropic-compatible base URL to the provider it routes to.
fn route_by_base(base: &str) -> Option<&'static str> {
    let b = base.to_ascii_lowercase();
    if b.contains("z.ai") || b.contains("bigmodel") {
        Some("glm")
    } else if b.contains("kimi.com") || b.contains("moonshot") {
        Some("kimi-code")
    } else if b.contains("deepseek") {
        Some("deepseek")
    } else {
        None
    }
}

fn opencode_auth() -> Option<(Value, String)> {
    let dir = env("OPENCODE_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| env("XDG_DATA_HOME").map(|x| PathBuf::from(x).join("opencode")))
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share/opencode")))?;
    let p = dir.join("auth.json");
    read_json(&p).map(|v| (v, p.display().to_string()))
}

/// Newest access_token among ~/.kimi/credentials/*.json (kimi-cli OAuth).
fn kimi_cli_token() -> Option<(String, String)> {
    let dir = env("KIMI_SHARE_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".kimi")))?
        .join("credentials");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") {
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                if newest.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                    newest = Some((t, p));
                }
            }
        }
    }
    let (_, p) = newest?;
    let tok = read_json(&p)?["access_token"].as_str()?.to_string();
    Some((tok, p.display().to_string()))
}

/// Claude Code settings.json env block (routed coding-plan setups).
fn claude_settings_env() -> Option<(String, String, String)> {
    for cand in [
        env("CLAUDE_CONFIG_DIR").map(|d| PathBuf::from(d).join("settings.json")),
        dirs::home_dir().map(|h| h.join(".claude/settings.json")),
        dirs::home_dir().map(|h| h.join(".config/claude/settings.json")),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(v) = read_json(&cand) {
            let e = &v["env"];
            if let (Some(tok), Some(base)) = (
                e["ANTHROPIC_AUTH_TOKEN"].as_str(),
                e["ANTHROPIC_BASE_URL"].as_str(),
            ) {
                return Some((tok.into(), base.into(), cand.display().to_string()));
            }
        }
    }
    None
}

/// Fill unset cfg fields from local sources; record (field, source).
pub fn apply(cfg: &mut Config) {
    let found = |cfg_found: &mut Vec<(String, String)>, field: &str, src: String| {
        cfg_found.push((field.into(), src));
    };

    // --- env vars that the plain getters don't already cover ---------
    if cfg.anthropic.admin_key.is_none() && env("ANTHROPIC_ADMIN_KEY").is_none() {
        if let Some(k) = env("ANTHROPIC_API_KEY").filter(|k| k.starts_with("sk-ant-admin")) {
            cfg.anthropic.admin_key = Some(k);
            found(
                &mut cfg.found,
                "anthropic.admin_key",
                "env ANTHROPIC_API_KEY (admin-grade)".into(),
            );
        }
    }
    if cfg.openai.admin_key.is_none() && env("OPENAI_ADMIN_KEY").is_none() {
        if let Some(k) = env("OPENAI_API_KEY").filter(|k| k.starts_with("sk-admin")) {
            cfg.openai.admin_key = Some(k);
            found(
                &mut cfg.found,
                "openai.admin_key",
                "env OPENAI_API_KEY (admin-grade)".into(),
            );
        }
    }

    // ANTHROPIC_AUTH_TOKEN + ANTHROPIC_BASE_URL in the process env.
    if let (Some(tok), Some(base)) = (env("ANTHROPIC_AUTH_TOKEN"), env("ANTHROPIC_BASE_URL")) {
        match route_by_base(&base) {
            Some("glm") if cfg.glm.api_key.is_none() && cfg.glm.key().is_none() => {
                cfg.glm.api_key = Some(tok);
                found(
                    &mut cfg.found,
                    "glm.api_key",
                    "env ANTHROPIC_AUTH_TOKEN -> z.ai".into(),
                );
            }
            Some("kimi-code") if cfg.kimi.code_key.is_none() && cfg.kimi.code_key().is_none() => {
                cfg.kimi.code_key = Some(tok);
                found(
                    &mut cfg.found,
                    "kimi.code_key",
                    "env ANTHROPIC_AUTH_TOKEN -> kimi".into(),
                );
            }
            Some("deepseek") if cfg.deepseek.api_key.is_none() => {
                cfg.deepseek.api_key = Some(tok);
                found(
                    &mut cfg.found,
                    "deepseek.api_key",
                    "env ANTHROPIC_AUTH_TOKEN -> deepseek".into(),
                );
            }
            _ => {}
        }
    }

    // --- Claude Code settings.json env block -------------------------
    if let Some((tok, base, src)) = claude_settings_env() {
        match route_by_base(&base) {
            Some("glm") if cfg.glm.key().is_none() => {
                cfg.glm.api_key = Some(tok);
                if base.contains("bigmodel") && cfg.glm.base_url.is_none() {
                    cfg.glm.base_url = Some("https://open.bigmodel.cn".into());
                }
                found(&mut cfg.found, "glm.api_key", src);
            }
            Some("kimi-code") if cfg.kimi.code_key().is_none() => {
                cfg.kimi.code_key = Some(tok);
                found(&mut cfg.found, "kimi.code_key", src);
            }
            Some("deepseek")
                if cfg.deepseek.api_key.is_none() && env("DEEPSEEK_API_KEY").is_none() =>
            {
                cfg.deepseek.api_key = Some(tok);
                found(&mut cfg.found, "deepseek.api_key", src);
            }
            _ => {}
        }
    }

    // --- OpenCode auth.json ------------------------------------------
    if let Some((auth, src)) = opencode_auth() {
        let api_key = |ids: &[&str]| {
            ids.iter().find_map(|id| {
                let e = &auth[*id];
                (e["type"].as_str() == Some("api"))
                    .then(|| e["key"].as_str())
                    .flatten()
                    .map(String::from)
            })
        };
        if cfg.glm.key().is_none() {
            if let Some(k) = api_key(&["zai", "zai-coding-plan", "zhipuai", "glm"]) {
                cfg.glm.api_key = Some(k);
                found(&mut cfg.found, "glm.api_key", src.clone());
            }
        }
        if cfg.deepseek.api_key.is_none() && env("DEEPSEEK_API_KEY").is_none() {
            if let Some(k) = api_key(&["deepseek"]) {
                cfg.deepseek.api_key = Some(k);
                found(&mut cfg.found, "deepseek.api_key", src.clone());
            }
        }
        if cfg.kimi.key().is_none() {
            if let Some(k) = api_key(&["moonshotai", "moonshot"]) {
                cfg.kimi.api_key = Some(k);
                found(&mut cfg.found, "kimi.api_key", src.clone());
            }
        }
        if cfg.kimi.code_key().is_none() {
            if let Some(k) = api_key(&["kimi-for-coding", "kimi"]) {
                cfg.kimi.code_key = Some(k);
                found(&mut cfg.found, "kimi.code_key", src.clone());
            }
        }
        // OpenCode's Claude Pro/Max OAuth access token: a fallback when
        // Claude Code's own credentials file is absent. Best-effort —
        // the token may lack the profile scope, in which case the quota
        // call fails gracefully.
        if cfg.claude.access_token.is_none() && cfg.claude.credentials_path().is_none() {
            let e = &auth["anthropic"];
            if e["type"].as_str() == Some("oauth") {
                if let Some(t) = e["access"].as_str() {
                    cfg.claude.access_token = Some(t.into());
                    found(&mut cfg.found, "claude.access_token", src.clone());
                }
            }
        }
    }

    // --- kimi-cli OAuth credentials ----------------------------------
    if cfg.kimi.code_key().is_none() {
        if let Some((tok, src)) = kimi_cli_token() {
            cfg.kimi.code_key = Some(tok);
            found(&mut cfg.found, "kimi.code_key", src);
        }
    }
}

/// Human string of env-var-configured fields (for `llmu providers`).
pub fn env_provenance() -> Vec<(String, String)> {
    let mut out = vec![];
    for (field, vars) in [
        ("anthropic.admin_key", &["ANTHROPIC_ADMIN_KEY"][..]),
        ("openai.admin_key", &["OPENAI_ADMIN_KEY"][..]),
        ("deepseek.api_key", &["DEEPSEEK_API_KEY"][..]),
        ("kimi.api_key", &["MOONSHOT_API_KEY", "KIMI_API_KEY"][..]),
        ("kimi.code_key", &["KIMI_CODE_API_KEY"][..]),
        ("glm.api_key", &["ZAI_API_KEY", "ZHIPU_API_KEY"][..]),
    ] {
        for v in vars {
            if env(v).is_some() {
                out.push((field.to_string(), format!("env {v}")));
            }
        }
    }
    out
}
