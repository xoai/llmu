//! Task 4: failing Gemini Code Assist quota source/config/endpoint contract
//! (RED).
//!
//! Std-only source contract, never importing private `llmu` modules — it
//! pins the FR-5 / AS-1 / AS-2 surface textually (mirroring the
//! `tests/http_cache.rs` pattern) so later tasks cannot drift the shape
//! this provider depends on. It fails because the contract does not exist
//! yet: no `[gemini]` credential/project overrides, no quota override, no
//! pinned refresh constants or corrected endpoints.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

/// The non-test implementation portion of a source file: tests may
/// legitimately assert the absence of a stale name in captured traffic,
/// so the strict name-absence pins (FR-5.6/5.7) apply to implementation
/// only.
fn impl_part(src: &str) -> &str {
    src.split("#[cfg(test)]").next().unwrap_or(src)
}

#[test]
fn config_gemini_gains_credential_and_project_overrides() {
    let cfg = read("src/config.rs");
    assert!(
        cfg.contains("pub struct GeminiCfg"),
        "src/config.rs must keep GeminiCfg (FR-5.1 `[gemini]`)"
    );
    assert!(
        cfg.contains("pub usage_log: Option<PathBuf>"),
        "the existing usage_log override must survive (FR-5.1 preserves usage behavior)"
    );
    assert!(
        cfg.contains("pub credentials: Option<PathBuf>"),
        "GeminiCfg must gain an optional `credentials` path override (FR-5.1)"
    );
    assert!(
        cfg.contains("pub project: Option<String>"),
        "GeminiCfg must gain an optional `project` override (FR-5.1)"
    );
}

#[test]
fn default_plaintext_credential_path_matches_gemini_cli() {
    let cfg = read("src/config.rs");
    assert!(
        cfg.contains("oauth_creds.json"),
        "the default credential file is Gemini CLI's `oauth_creds.json` (FR-5.1)"
    );
    assert!(
        cfg.contains(".gemini"),
        "the default credential directory is `~/.gemini` (FR-5.1)"
    );
    assert!(
        cfg.contains("GEMINI_CLI_HOME"),
        "GEMINI_CLI_HOME must override the home root: `${{GEMINI_CLI_HOME:-$HOME}}` (FR-5.1)"
    );
    assert!(
        cfg.contains("fn credentials_path"),
        "GeminiCfg must expose `credentials_path` for quota-configured detection (FR-5.2)"
    );
}

#[test]
fn encrypted_store_marker_is_diagnosed_not_mutated() {
    let cfg = read("src/config.rs");
    assert!(
        cfg.contains("gemini-credentials.json"),
        "sibling `gemini-credentials.json` is the Gemini CLI v0.39.1 encrypted/keychain marker (FR-5.2)"
    );
    assert!(
        cfg.contains("fn encrypted_marker_path"),
        "GeminiCfg must expose the encrypted-store marker check (FR-5.2)"
    );
}

#[test]
fn project_precedence_envs_are_declared() {
    let cfg = read("src/config.rs");
    for needle in ["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_PROJECT_ID"] {
        assert!(
            cfg.contains(needle),
            "project precedence must consult `{needle}` (FR-5.5: config > GOOGLE_CLOUD_PROJECT > GOOGLE_CLOUD_PROJECT_ID)"
        );
    }
}

#[test]
fn gemini_overrides_quotas_with_quota_fetch() {
    let gem = read("src/providers/gemini.rs");
    assert!(
        gem.contains("fn quotas("),
        "Gemini must override `quotas` in Task 4 (its first override)"
    );
    assert!(
        gem.contains("Result<QuotaFetch>"),
        "Gemini's quotas override must return `Result<QuotaFetch>` (AD-4)"
    );
    assert!(
        gem.contains("QuotaFetch::live"),
        "live observed quota rows must be marked for last-known-good refresh (AD-1)"
    );
}

#[test]
fn corrected_endpoints_are_used_and_onboarding_is_forbidden() {
    let gem = read("src/providers/gemini.rs");
    for needle in [
        "v1internal:loadCodeAssist",
        "v1internal:retrieveUserQuota",
        "oauth2.googleapis.com/token",
    ] {
        assert!(
            gem.contains(needle),
            "Gemini must call `{needle}` (FR-5.5/5.7: corrected endpoint names)"
        );
    }
    let impl_part = impl_part(&gem);
    assert!(
        !impl_part.contains("onboardUser"),
        "llmu never calls onboardUser — the name must not appear in the implementation (FR-5.6)"
    );
    assert!(
        !impl_part.contains("retrieveUserQuotaSummary"),
        "the stale retrieveUserQuotaSummary name must be absent from implementation (FR-5.7)"
    );
}

#[test]
fn refresh_pins_upstream_installed_app_identifiers() {
    let gem = read("src/providers/gemini.rs");
    assert!(
        gem.contains("681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"),
        "client_id must be the Gemini CLI v0.39.1 installed-app constant (FR-5.3)"
    );
    assert!(
        gem.contains("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"),
        "client_secret must be the public Gemini CLI v0.39.1 installed-app constant (FR-5.3)"
    );
    assert!(
        gem.contains("300_000"),
        "refresh must trigger within five minutes of expiry: 300_000 ms (FR-5.3)"
    );
    assert!(
        gem.contains("expires_in"),
        "expires_in seconds must convert to epoch milliseconds (FR-5.3)"
    );
}

#[test]
fn refresh_uses_the_locked_credential_transaction() {
    let gem = read("src/providers/gemini.rs");
    assert!(
        gem.contains("lock_and_read"),
        "refresh must acquire the llmu refresh lock BEFORE the HTTP request (FR-4.3, Task 2 API)"
    );
    assert!(
        gem.contains("replace_with_cas"),
        "persistence must consume the same LockedCredential guard via CAS replacement (FR-4.3)"
    );
    assert!(
        gem.contains("refresh_token"),
        "refresh must read and retain the refresh token (FR-5.3)"
    );
}

#[test]
fn quota_parser_normalizes_bucket_fields() {
    let gem = read("src/providers/gemini.rs");
    for needle in [
        "modelId",
        "tokenType",
        "remainingAmount",
        "remainingFraction",
        "resetTime",
    ] {
        assert!(
            gem.contains(needle),
            "the bucket parser must handle `{needle}` (FR-5.8)"
        );
    }
    assert!(
        gem.contains("100"),
        "fraction-only rows must normalize to a 100-unit percentage (FR-5.8)"
    );
}

#[test]
fn diagnostics_are_secret_free() {
    let gem = read("src/providers/gemini.rs");
    assert!(
        !gem.contains("println!") && !gem.contains("eprintln!"),
        "the provider never logs directly; diagnostics are notes/error values only (NFR Security)"
    );
}

#[test]
fn usage_log_behavior_is_preserved() {
    let gem = read("src/providers/gemini.rs");
    assert!(
        gem.contains("fn usage("),
        "the existing usageMetadata JSONL usage path must survive (FR-5.1 preserves usage behavior)"
    );
    assert!(
        gem.contains("usage_log"),
        "usage must keep reading the `usage_log` config key"
    );
    let cfg = read("src/config.rs");
    assert!(
        cfg.contains("pub usage_log: Option<PathBuf>"),
        "config must keep the usage_log field (FR-5.1)"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/gemini_quota.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/gemini_quota.rs is a std-only contract and must not import llmu"
    );
}
