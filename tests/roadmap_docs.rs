//! Task 9: documentation contracts (RED).
//!
//! Std-only, never importing private `llmu` modules — the established
//! `tests/*.rs` pattern. These contracts pin USER-VISIBLE documentation
//! facts (FR-7): the README roadmap status, CLI flags and exact CSV
//! schemas, offline history semantics, optional cache behavior, Gemini
//! and Claude credential boundaries, the `llmu providers` label, the
//! generated config sample, and the CHANGELOG Unreleased coverage. The
//! contracts fail if roadmap boxes regress, the stale Gemini endpoint
//! is presented as current, required flags/schemas disappear, or
//! unconditional read-only credential wording returns.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

/// Body of one `## `-headed section: everything up to the next heading.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    text.split(heading)
        .nth(1)
        .unwrap_or_else(|| panic!("missing section {heading}"))
        .split("## ")
        .next()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Roadmap status (FR-7.1)
// ---------------------------------------------------------------------------

#[test]
fn readme_roadmap_boxes_are_all_checked() {
    let readme = read("README.md");
    let roadmap = section(&readme, "## Roadmap");
    let boxes: Vec<&str> = roadmap
        .lines()
        .filter(|l| l.trim_start().starts_with("- ["))
        .collect();
    assert!(
        boxes.len() >= 4,
        "roadmap must still list the remaining items (FR-7.1)"
    );
    for b in &boxes {
        assert!(
            b.contains("[x]"),
            "every roadmap box must be checked: {b} (FR-7.1)"
        );
    }
}

#[test]
fn readme_roadmap_covers_all_five_features_with_corrected_endpoint() {
    let readme = read("README.md");
    let roadmap = section(&readme, "## Roadmap");
    for needle in [
        "Gemini",
        "retrieveUserQuota",
        "auto-refresh",
        "balance --history",
        "--csv",
        "cache",
    ] {
        assert!(
            roadmap.contains(needle),
            "roadmap must mention {needle} (FR-7.1)"
        );
    }
    let readme = read("README.md");
    assert!(
        !readme.contains("retrieveUserQuotaSummary"),
        "the stale Gemini endpoint name must never be presented as current (FR-7.1)"
    );
}

// ---------------------------------------------------------------------------
// CLI flags, CSV schemas, and output-mode conflicts (FR-1, FR-7.1)
// ---------------------------------------------------------------------------

#[test]
fn readme_documents_csv_schemas_and_conflict() {
    let readme = read("README.md");
    for needle in [
        "--csv",
        "RFC 4180",
        "period",
        "requests",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "total_tokens",
        "tool_calls",
        "est_cost_usd",
        "has_cost",
        "provider,total,granted,topped_up,currency",
        "provider,plan,window,used,limit,unit,resets_at",
        "from,to,provider,currency,opening,closing,spent,funded",
    ] {
        assert!(
            readme.contains(needle),
            "README must document the exact CSV surface including {needle} (FR-7.1/FR-1)"
        );
    }
    assert!(
        readme.contains("--json") && readme.contains("reject"),
        "README must document that clap rejects --json --csv (FR-1.2)"
    );
    assert!(
        readme.contains("header"),
        "README must document the empty-result header behavior (FR-1.7)"
    );
}

#[test]
fn readme_documents_fresh_flag_and_tui_bypass_lifetime() {
    let readme = read("README.md");
    assert!(
        readme.contains("--fresh"),
        "the global --fresh flag must be documented (FR-3.2/FR-7.1)"
    );
    let watch = section(&readme, "## Live watch mode");
    assert!(
        watch.contains("initial"),
        "TUI --fresh applies only to the initial full network fetch (FR-3.2)"
    );
    assert!(
        watch.contains("bypass"),
        "the r keypress raw-cache bypass must be documented (FR-3.2)"
    );
}

// ---------------------------------------------------------------------------
// Optional HTTP response cache (FR-3, FR-7.1)
// ---------------------------------------------------------------------------

#[test]
fn readme_documents_http_cache_semantics() {
    let readme = read("README.md");
    let section = section(&readme, "## Optional HTTP response cache");
    for needle in [
        "[http_cache]",
        "ttl_seconds = 0",
        "SHA-256",
        "GET",
        "OAuth",
        "llmu/http",
        "Authorization",
        "live request",
    ] {
        assert!(
            section.contains(needle),
            "the cache section must document {needle} (FR-3/FR-7.1)"
        );
    }
}

// ---------------------------------------------------------------------------
// Offline balance history (FR-2, FR-7.1)
// ---------------------------------------------------------------------------

#[test]
fn readme_documents_offline_history_semantics() {
    let readme = read("README.md");
    let section = section(&readme, "## Offline balance history");
    for needle in [
        "balances.jsonl",
        "UTC",
        "daily close",
        "no provider requests",
        "appends no snapshot",
        "spent",
        "funded",
        "malformed",
        "stderr",
    ] {
        assert!(
            section.contains(needle),
            "the history section must document {needle} (FR-2/FR-7.1)"
        );
    }
}

// ---------------------------------------------------------------------------
// Credential mutation boundaries (FR-4, FR-7.1/7.3/7.4)
// ---------------------------------------------------------------------------

#[test]
fn readme_scopes_the_credential_read_only_promise() {
    let readme = read("README.md");
    assert!(
        !readme.contains("never written"),
        "README must not claim credentials are never written (FR-7.1 mutation caveat)"
    );
    let zero = section(&readme, "## Zero config");
    assert!(
        zero.contains("refresh") && zero.contains("exception"),
        "zero-config must name the read-only discovery exception (FR-7.1)"
    );
    assert!(
        zero.contains("oauth_creds.json") || zero.contains("OAuth"),
        "zero-config must name the supported OAuth credential stores"
    );
}

#[test]
fn providers_command_wording_names_the_refresh_exception() {
    let m = read("src/main.rs");
    let prov = m
        .split("Cmd::Providers =>")
        .nth(1)
        .expect("providers arm in main.rs");
    let label = prov
        .lines()
        .find(|l| l.contains("auto-detected credentials"))
        .expect("auto-detected credentials label");
    assert!(
        label.contains("read-only"),
        "the label must keep the read-only base (FR-7.4)"
    );
    assert!(
        label.contains("refresh"),
        "the label must name the supported OAuth refresh exception (FR-7.4)"
    );
    assert!(
        !label.contains("(read-only):"),
        "unconditional read-only wording must not return (FR-7.4)"
    );
}

#[test]
fn discover_docs_scope_the_read_only_promise() {
    let d = read("src/discover.rs");
    let head: String = d.lines().take(30).collect::<Vec<_>>().join("\n");
    assert!(
        !head.contains("Everything is READ-ONLY"),
        "the unconditional read-only claim must be scoped (FR-7.4)"
    );
    assert!(
        head.contains("refresh"),
        "discovery docs must name the supported OAuth refresh exception (FR-7.4)"
    );
}

#[test]
fn config_sample_documents_cache_gemini_and_mutation_boundary() {
    let cfg = read("src/config.rs");
    let sample = cfg
        .split("pub fn sample")
        .nth(1)
        .expect("Config::sample body");
    for needle in ["[http_cache]", "ttl_seconds", "oauth_creds.json", "GOOGLE_CLOUD_PROJECT"] {
        assert!(
            sample.contains(needle),
            "Config::sample must document {needle} (FR-7.3)"
        );
    }
    assert!(
        sample.contains("read-only"),
        "Config::sample must state the read-only-except-refresh rule (FR-7.3)"
    );
    assert!(
        sample.contains("encrypted"),
        "Config::sample must note unsupported encrypted/keychain stores (FR-7.3)"
    );
}

// ---------------------------------------------------------------------------
// Gemini and Claude documentation (FR-5, FR-6, FR-7.1/7.2)
// ---------------------------------------------------------------------------

#[test]
fn readme_documents_gemini_and_claude_credential_boundaries() {
    let readme = read("README.md");
    assert!(
        readme.contains("oauth_creds.json"),
        "README must name Gemini's plaintext credential file (FR-7.1)"
    );
    assert!(
        readme.contains("encrypted"),
        "README must note encrypted/keychain Gemini storage is unsupported (FR-7.1)"
    );
    let troubleshooting = section(&readme, "## Troubleshooting");
    assert!(
        troubleshooting.contains("OpenCode"),
        "README must keep the OpenCode access-only remediation (FR-6.8)"
    );
}

#[test]
fn providers_doc_gemini_is_wired_with_corrected_endpoint() {
    let p = read("docs/providers.md");
    assert!(
        p.contains("v1internal:loadCodeAssist"),
        "docs must document loadCodeAssist (FR-5.5)"
    );
    assert!(
        p.contains("v1internal:retrieveUserQuota"),
        "docs must document the corrected retrieveUserQuota endpoint (FR-5.7)"
    );
    let gem = section(&p, "### Gemini CLI / Code Assist");
    assert!(
        gem.contains("wired"),
        "the Gemini section must no longer claim 'not wired' (FR-7.2)"
    );
    assert!(
        !gem.contains("not wired"),
        "the Gemini section must not claim to be unwired (FR-7.2)"
    );
    assert!(
        gem.contains("oauth_creds.json"),
        "docs must name Gemini's plaintext credential file (FR-5.1)"
    );
    assert!(
        gem.contains("refresh"),
        "docs must document the Gemini OAuth refresh flow (FR-5.3)"
    );
    if let Some(idx) = p.find("retrieveUserQuotaSummary") {
        let before = &p[..idx];
        let ctx = &before[before.len().saturating_sub(200)..];
        assert!(
            ctx.contains("migration")
                || ctx.contains("stale")
                || ctx.contains("previously")
                || ctx.contains("renamed"),
            "the stale retrieveUserQuotaSummary name may appear only in a migration explanation (FR-5.7)"
        );
    }
}

#[test]
fn providers_doc_claude_refresh_boundaries_are_exact() {
    let p = read("docs/providers.md");
    let claude = section(&p, "### Claude Pro/Max — live subscription meters");
    for needle in [
        "five minutes",
        "refresh_token",
        "rotated",
        "401",
        "three days",
        "OpenCode",
        "invalid_grant",
        "log in again",
    ] {
        assert!(
            claude.contains(needle),
            "the Claude section must cover {needle} (FR-6/FR-7.2)"
        );
    }
}

#[test]
fn public_oauth_identifiers_are_labeled_as_public() {
    let p = read("docs/providers.md");
    for needle in [
        "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com",
        "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    ] {
        let idx = p
            .find(needle)
            .unwrap_or_else(|| panic!("public installed-app id must be documented: {needle}"));
        let window = idx.saturating_sub(120)..(idx + needle.len() + 120).min(p.len());
        assert!(
            p[window].contains("public"),
            "public OAuth identifiers must be labeled as public upstream constants (NFR Security): {needle}"
        );
    }
}

// ---------------------------------------------------------------------------
// CHANGELOG coverage (FR-7)
// ---------------------------------------------------------------------------

#[test]
fn changelog_unreleased_covers_roadmap_features_and_constraints() {
    let c = read("CHANGELOG.md");
    let unrel = section(&c, "## [Unreleased]");
    for needle in [
        "Gemini",
        "retrieveUserQuota",
        "Claude",
        "refresh",
        "balance --history",
        "--csv",
        "cache",
        "--fresh",
        "read-only",
    ] {
        assert!(
            unrel.contains(needle),
            "CHANGELOG Unreleased must cover {needle} (FR-7)"
        );
    }
}

// ---------------------------------------------------------------------------
// Release/install docs stay correct (verify-only)
// ---------------------------------------------------------------------------

#[test]
fn release_and_install_docs_remain_correct() {
    let readme = read("README.md");
    assert!(
        readme.contains("Rust >= 1.75"),
        "the MSRV claim must survive"
    );
    assert!(
        readme.contains("Release Please"),
        "the release-process documentation must survive"
    );
    assert!(
        readme.contains("crates.io") && readme.contains("not published"),
        "the crates.io non-publication claim must survive"
    );
    let changelog = read("CHANGELOG.md");
    assert!(
        changelog.contains("[Keep a Changelog]"),
        "the changelog format header must survive"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/roadmap_docs.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/roadmap_docs.rs is a std-only contract and must not import llmu"
    );
}
