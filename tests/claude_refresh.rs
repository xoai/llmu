//! Task 5: failing Claude OAuth auto-refresh source/endpoint contract (RED).
//!
//! Std-only source contract, never importing private `llmu` modules — it
//! pins the FR-6 surface textually (mirroring `tests/credentials.rs`) so
//! the Claude refresh flow cannot drift the shape Tasks 7/9 depend on. It
//! fails because the contract does not exist yet: no pinned production
//! client id, no proactive five-minute refresh horizon, no locked refresh
//! transaction, no one-shot 401 retry, no OpenCode-specific remediation.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

#[test]
fn claude_sub_pins_production_oauth_client_id_and_token_endpoint() {
    let c = read("src/providers/claude_sub.rs");
    assert!(
        c.contains("9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
        "the pinned production client_id from Claude Code 2.1.227 must live in the provider (FR-6.3)"
    );
    assert!(
        c.contains("platform.claude.com/v1/oauth/token"),
        "the refresh POST endpoint must be platform.claude.com/v1/oauth/token (FR-6.3)"
    );
}

#[test]
fn proactive_refresh_horizon_is_five_minutes() {
    let c = read("src/providers/claude_sub.rs");
    assert!(
        c.contains("300_000"),
        "the proactive refresh lead must be 300000ms (FR-6.2: expiresAt <= now + 300000)"
    );
    assert!(
        c.contains("PROACTIVE_LEAD_MS"),
        "the lead must be a named constant"
    );
}

#[test]
fn refresh_uses_the_shared_locked_transaction() {
    let c = read("src/providers/claude_sub.rs");
    for needle in [
        "lock_and_read",
        "replace_with_cas",
        "LockTiming",
        "refresh_or_adopt",
    ] {
        assert!(
            c.contains(needle),
            "src/providers/claude_sub.rs must use `{needle}` — the refresh request is only sent after the llmu lock is held (FR-4.3, FR-6.2)"
        );
    }
}

#[test]
fn token_payload_and_schema_fields_are_pinned() {
    let c = read("src/providers/claude_sub.rs");
    for needle in [
        "accessToken",
        "refreshToken",
        "expiresAt",
        "refreshTokenExpiresAt",
        "scopes",
        "clientId",
        "subscriptionType",
        "rateLimitTier",
    ] {
        assert!(
            c.contains(needle),
            "src/providers/claude_sub.rs must read `{needle}` from claudeAiOauth (FR-6.1)"
        );
    }
    assert!(
        !c.contains("client_secret"),
        "the refresh POST must never carry a client secret (FR-6.3)"
    );
}

#[test]
fn usage_401_gets_a_bounded_reactive_retry() {
    let c = read("src/providers/claude_sub.rs");
    for needle in ["usage_is_401", "refresh_or_adopt"] {
        assert!(
            c.contains(needle),
            "src/providers/claude_sub.rs must `{needle}` — a usage 401 re-reads, adopts a changed token, or forces exactly one refresh and retries once (FR-6.6)"
        );
    }
    assert!(
        c.contains("401") && c.contains("retry"),
        "the reactive path must be visibly 401-gated"
    );
}

#[test]
fn opencode_direct_token_401_remediation_names_the_source() {
    let c = read("src/providers/claude_sub.rs");
    assert!(
        c.contains("OpenCode"),
        "an access-only 401 must name OpenCode as the token source (FR-6.8)"
    );
    assert!(
        c.contains("cannot refresh"),
        "the message must not claim llmu can refresh a direct access token (FR-6.8)"
    );
}

#[test]
fn refresh_token_expiry_warning_horizon_is_three_days() {
    let c = read("src/providers/claude_sub.rs");
    assert!(
        c.contains("three days"),
        "a refresh-token expiry within three days must warn without blocking (FR-6.9)"
    );
    assert!(
        c.contains("3 * 24 * 60 * 60 * 1000") || c.contains("259_200_000"),
        "the warning horizon must be three days in milliseconds"
    );
}

#[test]
fn exact_429_messaging_and_quota_provenance_are_retained() {
    let c = read("src/providers/claude_sub.rs");
    assert!(
        c.contains(
            "oauth/usage rate-limited (this endpoint throttles hard) — retry in a few minutes"
        ),
        "the existing exact 429 message must survive (FR-6.10)"
    );
    assert!(
        c.contains("Result<QuotaFetch>"),
        "quotas must return the Task 3 QuotaFetch (AD-4 provenance)"
    );
    assert!(
        c.contains("refresh_last_known_good"),
        "live rows must mark last-known-good refresh"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/claude_refresh.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/claude_refresh.rs is a std-only contract and must not import llmu"
    );
}
