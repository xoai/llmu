//! Zero-config credential discovery.
//!
//! Most of what llmu needs already exists on the machine, written by the
//! CLIs people use. On every run, unset config fields are filled from
//! (in order): explicit config.toml > env vars > credential files of
//! other tools. Discovery is READ-ONLY and best-effort — llmu never
//! creates, deletes, or overwrites a credential — with one explicit
//! exception: a supported plaintext Gemini CLI / Claude Code OAuth file
//! may be refreshed (rotated tokens persisted atomically, unknown fields
//! preserved, mode 0600, llmu-only lock plus compare-and-swap; see
//! docs/providers.md). Access-only OpenCode tokens, encrypted stores,
//! and keychains are never mutated. Provenance is recorded so
//! `llmu providers` can show where each credential came from.
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
//! - Qwen: `${QWEN_HOME:-~/.qwen}/settings.json` `env` block (standard
//!   `DASHSCOPE_API_KEY` then `BAILIAN_API_KEY`, Coding Plan
//!   `BAILIAN_CODING_PLAN_API_KEY`, Token Plan
//!   `BAILIAN_TOKEN_PLAN_API_KEY`) plus `advanced.runtimeOutputDir`,
//!   with `QWEN_HOME` / `QWEN_RUNTIME_DIR` precedence overrides; strictly
//!   read-only and network-free (FR-2).

use crate::config::{
    qwen_coding_plan_key, qwen_home, qwen_runtime, qwen_standard_key, qwen_token_plan_key, Config,
    EnvLookup, QwenSettings,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn env(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Process-environment lookup for the Qwen discovery seam; tests inject
/// sandboxed lookups instead (hermetic discovery).
fn std_env(k: &str) -> Option<String> {
    env(k)
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

/// Read `${home}/settings.json` into typed values (FR-2). `None` for a
/// missing, unreadable, or malformed/wrong-typed document — the file is
/// never created, modified, or parsed past the first failure (AC-10).
fn read_qwen_settings(home: &Path) -> Option<QwenSettings> {
    let raw = std::fs::read_to_string(home.join("settings.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Record field provenance for auto-detected Qwen values: only env and
/// settings sources are shown; explicit config and derived defaults are
/// not "auto-detected" and stay silent.
fn record_found(cfg: &mut Config, field: &str, src: String) {
    if src.starts_with("env ") || src.starts_with("settings ") {
        cfg.found.push((field.to_string(), src));
    }
}

/// Fill unset `[qwen]` fields from the injected environment lookup and
/// Qwen Code's `${home}/settings.json` (FR-1, FR-2). Read-only and
/// isolated: missing/unreadable/malformed settings change nothing and
/// cannot break unrelated providers (AC-10). The Qwen home/runtime
/// overrides may be pinned by explicit `Config` values so tests never
/// invoke real-home Qwen discovery.
fn apply_qwen(cfg: &mut Config, env: EnvLookup) {
    let Some((home, home_src)) = qwen_home(
        cfg.qwen.home.as_deref(),
        env,
        dirs::home_dir().map(|h| h.join(".qwen")).as_deref(),
    ) else {
        return;
    };
    let settings_src = home.join("settings.json").display().to_string();
    let settings = read_qwen_settings(&home);
    let empty_settings = QwenSettings::default();
    let settings = settings.as_ref().unwrap_or(&empty_settings);

    if cfg.qwen.home.is_none() {
        cfg.qwen.home = Some(home);
        record_found(cfg, "qwen.home", home_src);
    }
    if cfg.qwen.standard_key.is_none() {
        if let Some((v, src)) = qwen_standard_key(None, env, settings, &settings_src) {
            cfg.qwen.standard_key = Some(v);
            record_found(cfg, "qwen.standard_key", src);
        }
    }
    if cfg.qwen.coding_plan_key.is_none() {
        if let Some((v, src)) = qwen_coding_plan_key(None, env, settings, &settings_src) {
            cfg.qwen.coding_plan_key = Some(v);
            record_found(cfg, "qwen.coding_plan_key", src);
        }
    }
    if cfg.qwen.token_plan_key.is_none() {
        if let Some((v, src)) = qwen_token_plan_key(None, env, settings, &settings_src) {
            cfg.qwen.token_plan_key = Some(v);
            record_found(cfg, "qwen.token_plan_key", src);
        }
    }
    if cfg.qwen.runtime_dir.is_none() {
        if let Some((p, src)) = qwen_runtime(
            None,
            env,
            settings.advanced.runtime_output_dir.as_deref(),
            &settings_src,
            cfg.qwen
                .home
                .as_deref()
                .expect("qwen home is installed above"),
        ) {
            cfg.qwen.runtime_dir = Some(p);
            record_found(cfg, "qwen.runtime_dir", src);
        }
    }
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

    // --- Qwen Cloud: env + Qwen Code settings.json (FR-1, FR-2) ------
    apply_qwen(cfg, &std_env);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AnthropicCfg, QwenCfg};

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "llmu-qwen-discover-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn no_env(_k: &str) -> Option<String> {
        None
    }

    #[test]
    fn read_qwen_settings_missing_or_malformed_is_none() {
        let dir = temp_dir("missing");
        assert!(
            read_qwen_settings(&dir).is_none(),
            "a missing settings.json yields no settings, never an error (FR-2)"
        );
        std::fs::write(dir.join("settings.json"), "not json {{{").unwrap();
        assert!(
            read_qwen_settings(&dir).is_none(),
            "malformed settings yield no settings (FR-2, AC-10)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_qwen_settings_wrong_typed_values_is_none() {
        let dir = temp_dir("wrongtype");
        std::fs::write(dir.join("settings.json"), r#"{"env":"oops"}"#).unwrap();
        assert!(
            read_qwen_settings(&dir).is_none(),
            "a string `env` is wrong-typed (AC-10)"
        );
        std::fs::write(
            dir.join("settings.json"),
            r#"{"env":{"DASHSCOPE_API_KEY":42}}"#,
        )
        .unwrap();
        assert!(
            read_qwen_settings(&dir).is_none(),
            "a numeric key is wrong-typed (AC-10)"
        );
        std::fs::write(
            dir.join("settings.json"),
            r#"{"advanced":{"runtimeOutputDir":7}}"#,
        )
        .unwrap();
        assert!(
            read_qwen_settings(&dir).is_none(),
            "a numeric runtimeOutputDir is wrong-typed (AC-10)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_qwen_settings_valid_document_parses_exact_values() {
        let dir = temp_dir("valid");
        std::fs::write(
            dir.join("settings.json"),
            r#"{"env":{"DASHSCOPE_API_KEY":"sk-std","BAILIAN_CODING_PLAN_API_KEY":"sk-plan"},"advanced":{"runtimeOutputDir":"runtime"},"unrelated":true}"#,
        )
        .unwrap();
        let s = read_qwen_settings(&dir).expect("valid settings parse");
        assert_eq!(
            s.env.get("DASHSCOPE_API_KEY").map(String::as_str),
            Some("sk-std")
        );
        assert_eq!(
            s.env.get("BAILIAN_CODING_PLAN_API_KEY").map(String::as_str),
            Some("sk-plan")
        );
        assert_eq!(s.advanced.runtime_output_dir.as_deref(), Some("runtime"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hermetic `discover::apply` integration: Qwen home is pinned through
    /// an explicit `Config` temp-directory override, the process
    /// environment is never mutated (injected env lookup), a temp
    /// `settings.json` is read, and the resolved fields plus provenance
    /// are installed without invoking real-home Qwen discovery.
    #[test]
    fn apply_qwen_hermetic_integration_pins_home_and_records_provenance() {
        let dir = temp_dir("apply");
        let home = dir.join("qwen-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"env":{"DASHSCOPE_API_KEY":"sk-settings-std","BAILIAN_CODING_PLAN_API_KEY":"sk-settings-coding","BAILIAN_TOKEN_PLAN_API_KEY":"sk-settings-token"},"advanced":{"runtimeOutputDir":"rel/runtime"}}"#,
        )
        .unwrap();
        let env_pairs = [
            ("DASHSCOPE_API_KEY", "sk-env-std"),
            ("QWEN_RUNTIME_DIR", "env-runtime"),
        ];
        let env = |k: &str| {
            env_pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        };

        let mut cfg = Config {
            qwen: QwenCfg {
                home: Some(home.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        apply_qwen(&mut cfg, &env);

        assert_eq!(
            cfg.qwen.standard_key.as_deref(),
            Some("sk-env-std"),
            "explicit env beats settings for the standard key (FR-1)"
        );
        assert_eq!(
            cfg.qwen.coding_plan_key.as_deref(),
            Some("sk-settings-coding"),
            "Coding Plan fills from its settings key (FR-2)"
        );
        assert_eq!(
            cfg.qwen.token_plan_key.as_deref(),
            Some("sk-settings-token"),
            "Token Plan fills from its settings key (FR-2)"
        );
        assert_eq!(
            cfg.qwen.home.as_deref(),
            Some(home.as_path()),
            "explicit home is installed"
        );
        assert_eq!(
            cfg.qwen.runtime_dir.as_deref(),
            Some(Path::new("env-runtime")),
            "QWEN_RUNTIME_DIR precedes settings runtimeOutputDir (FR-1)"
        );

        let settings_path = home.join("settings.json").display().to_string();
        let fields: Vec<&str> = cfg.found.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(
            fields,
            vec![
                "qwen.standard_key",
                "qwen.coding_plan_key",
                "qwen.token_plan_key",
                "qwen.runtime_dir",
            ],
            "provenance is recorded in a deterministic field order"
        );
        assert!(
            cfg.found
                .iter()
                .any(|(f, s)| f == "qwen.standard_key" && s == "env DASHSCOPE_API_KEY"),
            "env-sourced keys name the variable"
        );
        for f in ["qwen.coding_plan_key", "qwen.token_plan_key"] {
            assert!(
                cfg.found
                    .iter()
                    .any(|(field, s)| field == f && s == &format!("settings {settings_path}")),
                "settings-sourced keys name the settings path (FR-2): {f}"
            );
        }
        assert!(
            cfg.found
                .iter()
                .any(|(f, s)| f == "qwen.runtime_dir" && s == "env QWEN_RUNTIME_DIR"),
            "env-sourced runtime names the variable"
        );
        assert!(
            !cfg.found.iter().any(|(f, _)| f == "qwen.home"),
            "an explicit config home is not 'auto-detected'"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_qwen_records_env_home_provenance() {
        let dir = temp_dir("envhome");
        let home = dir.join("qwen-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-tok"}}"#,
        )
        .unwrap();
        let env_pairs = [("QWEN_HOME", home.to_str().unwrap())];
        let env = |k: &str| {
            env_pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        };

        let mut cfg = Config::default();
        apply_qwen(&mut cfg, &env);

        assert_eq!(cfg.qwen.home.as_deref(), Some(home.as_path()));
        assert_eq!(cfg.qwen.token_plan_key.as_deref(), Some("sk-tok"));
        assert!(
            cfg.found
                .iter()
                .any(|(f, s)| f == "qwen.home" && s == "env QWEN_HOME"),
            "an env-sourced home names the variable (FR-1)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// AC-10: malformed settings isolate every Qwen field and leave
    /// unrelated provider fields untouched.
    #[test]
    fn apply_qwen_malformed_settings_isolate_all_qwen_fields() {
        let dir = temp_dir("malformed");
        let home = dir.join("qwen-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("settings.json"), "not json {{").unwrap();

        let mut cfg = Config {
            qwen: QwenCfg {
                home: Some(home.clone()),
                ..Default::default()
            },
            anthropic: AnthropicCfg {
                admin_key: Some("sk-ant-admin-x".into()),
            },
            ..Default::default()
        };
        apply_qwen(&mut cfg, &no_env);

        assert!(cfg.qwen.standard_key.is_none());
        assert!(cfg.qwen.coding_plan_key.is_none());
        assert!(cfg.qwen.token_plan_key.is_none());
        assert_eq!(
            cfg.qwen.home.as_deref(),
            Some(home.as_path()),
            "pinned home survives"
        );
        assert_eq!(
            cfg.qwen.runtime_dir.as_deref(),
            Some(home.as_path()),
            "effective home is the runtime fallback (FR-1)"
        );
        assert!(
            cfg.found.is_empty(),
            "malformed settings produce no provenance and no diagnostics (AC-10)"
        );
        assert_eq!(
            cfg.anthropic.admin_key.as_deref(),
            Some("sk-ant-admin-x"),
            "unrelated providers load untouched (AC-10)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
