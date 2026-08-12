//! Task 4: Qwen documentation and release contracts (RED).
//!
//! Std-only, never importing private `llmu` modules — the established
//! `tests/*.rs` pattern (files on disk only: no network, no real home,
//! no process-env mutation). These contracts pin USER-VISIBLE Qwen
//! documentation facts (FR-7, AC-9): the README provider list, zero-config
//! sources, data-availability matrix row, configuration, CLI reference,
//! panel counts, and troubleshooting; a dedicated implemented Qwen section
//! in `docs/providers.md` with the exact three key classes, env names,
//! host families, paths and precedence; the generated `Config::sample`
//! `[qwen]` section; `src/discover.rs` module-doc source list; the clap
//! `--provider` enumeration; the changelog release-section coverage; and the
//! no-realistic-secrets rule. The contracts fail when any of those
//! surfaces is missing, stale, or disagrees with the approved spec
//! (FR-1 through FR-7).

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

/// Owned body of a README `## `-headed section.
fn readme_section(heading: &str) -> String {
    section(&read("README.md"), heading).to_string()
}

/// Owned body of the `## Qwen` section of docs/providers.md.
fn qwen_section() -> String {
    section(&read("docs/providers.md"), "## Qwen").to_string()
}

/// `Config::sample()` body: everything after the `pub fn sample` marker.
fn sample_body() -> String {
    let cfg = read("src/config.rs");
    cfg.split("pub fn sample")
        .nth(1)
        .expect("Config::sample body")
        .to_string()
}

/// True when `text` contains a realistic `sk-…` key string: the `sk-`
/// prefix followed by 16+ ASCII alphanumerics. Truncated forms
/// (`sk-ant-admin…`), wildcard prefixes (`sk-sp-*`), and empty/commented
/// placeholders are not realistic keys.
fn has_realistic_key(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i..i + 3] == *b"sk-" {
            let n = text[i + 3..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .count();
            if n >= 16 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// README: provider list, zero-config sources, data matrix, config, CLI
// reference, panel counts, troubleshooting (FR-6, FR-7.1).
// ---------------------------------------------------------------------------

#[test]
fn readme_intro_lists_qwen() {
    let readme = read("README.md");
    let intro = readme.split("## Demo").next().unwrap(); // everything above the demo image
    assert!(
        intro.contains("Qwen"),
        "the README intro must list Qwen among the supported providers (FR-6/FR-7.1)"
    );
}

#[test]
fn readme_zero_config_tables_qwen_code_sources() {
    let zero = readme_section("## Zero config");
    assert!(
        zero.contains("~/.qwen/settings.json"),
        "zero-config must list Qwen Code's settings.json as a detected source (FR-7.1)"
    );
    for var in [
        "DASHSCOPE_API_KEY",
        "BAILIAN_API_KEY",
        "BAILIAN_CODING_PLAN_API_KEY",
        "BAILIAN_TOKEN_PLAN_API_KEY",
        "QWEN_HOME",
        "QWEN_RUNTIME_DIR",
    ] {
        assert!(
            zero.contains(var),
            "zero-config must name the env var {var} (FR-7.1)"
        );
    }
    assert!(
        zero.contains("console-only"),
        "zero-config must state QwenCloud account quota/billing is console-only (FR-7.1)"
    );
}

#[test]
fn readme_data_matrix_has_an_honest_qwen_row() {
    let matrix = readme_section("## The honest data-availability matrix");
    let row = matrix
        .lines()
        .find(|l| l.trim_start().starts_with("| Qwen"))
        .unwrap_or_else(|| panic!("the data matrix must include a Qwen row (FR-7.1): {matrix}"));
    assert!(
        row.contains("Qwen Code") && row.contains("local"),
        "the Qwen usage cell must name the local Qwen Code records source (FR-7.1): {row}"
    );
    assert!(
        row.contains("console-only"),
        "the Qwen row must mark subscription/analytics as console-only, not silently absent (FR-7.1): {row}"
    );
    assert!(
        !row.contains("Cost API"),
        "the Qwen row must not claim a billed-cost API (FR-7.1): {row}"
    );
}

#[test]
fn readme_configuration_documents_qwen_precedence() {
    let cfg = readme_section("## Configuration");
    assert!(
        cfg.contains("[qwen]"),
        "README configuration must document the [qwen] section (FR-7.1)"
    );
    for needle in [
        "DASHSCOPE_API_KEY",
        "BAILIAN_CODING_PLAN_API_KEY",
        "BAILIAN_TOKEN_PLAN_API_KEY",
        "QWEN_HOME",
        "QWEN_RUNTIME_DIR",
        "~/.qwen",
        "runtimeOutputDir",
    ] {
        assert!(
            cfg.contains(needle),
            "README configuration must document {needle} (FR-7.1)"
        );
    }
    assert!(
        cfg.contains("not interchangeable"),
        "README must state the three Qwen key classes are not interchangeable (FR-1/FR-7.1)"
    );
    assert!(
        cfg.contains("no built-in Qwen price"),
        "README must state llmu adds no built-in Qwen price guesses (FR-7.1)"
    );
}

#[test]
fn readme_cli_reference_enumerates_qwen() {
    let cli = readme_section("## CLI reference");
    assert!(
        cli.contains("qwen"),
        "the CLI reference --provider example must include qwen (FR-6/FR-7.1)"
    );
}

#[test]
fn readme_panel_counts_include_qwen() {
    let panels = readme_section("## What each panel counts");
    assert!(
        panels.contains("Qwen"),
        "panel-count docs must say where Qwen usage comes from (FR-6/FR-7.1)"
    );
    assert!(
        panels.contains("qwen") && panels.contains("ledger"),
        "panel-count docs must name Qwen Code's local ledgers as a usage feed (FR-7.1)"
    );
}

#[test]
fn readme_troubleshooting_covers_qwen() {
    let troubleshooting = readme_section("## Troubleshooting");
    assert!(
        troubleshooting.contains("qwen:"),
        "troubleshooting must explain qwen diagnostics (e.g. skipped local records) (FR-7.1)"
    );
    assert!(
        troubleshooting.contains("console-only") || troubleshooting.contains("local records"),
        "troubleshooting must explain key-only Qwen setups report no account usage (FR-7.1)"
    );
}

// ---------------------------------------------------------------------------
// docs/providers.md: dedicated implemented Qwen section with exact key
// classes, env names, host families, paths, precedence, and trust boundary
// (FR-7.2, AC-9).
// ---------------------------------------------------------------------------

#[test]
fn providers_doc_has_an_implemented_qwen_section() {
    let qwen = qwen_section();
    assert!(
        qwen.contains("implemented"),
        "the Qwen section must state its implemented status (FR-7.2)"
    );
    assert!(
        qwen.contains("local"),
        "the Qwen section must state usage is read from local Qwen Code records (FR-7.2)"
    );
    assert!(
        qwen.contains("console-only"),
        "the Qwen section must mark QwenCloud account quota/billing console-only (FR-7.2)"
    );
}

#[test]
fn providers_doc_lists_three_key_classes_with_exact_envs() {
    let qwen = qwen_section();
    for needle in [
        "Standard",
        "Coding Plan",
        "Token Plan",
        "DASHSCOPE_API_KEY",
        "BAILIAN_API_KEY",
        "BAILIAN_CODING_PLAN_API_KEY",
        "BAILIAN_TOKEN_PLAN_API_KEY",
    ] {
        assert!(
            qwen.contains(needle),
            "the Qwen section must document {needle} (FR-1/FR-7.2)"
        );
    }
    assert!(
        qwen.contains("not interchangeable"),
        "the Qwen section must state the key classes and base URLs are not interchangeable (FR-1/FR-7.2)"
    );
}

#[test]
fn providers_doc_lists_exact_host_families() {
    let qwen = qwen_section();
    for host in [
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        "https://dashscope-us.aliyuncs.com/compatible-mode/v1",
        "https://cn-hongkong.dashscope.aliyuncs.com/compatible-mode/v1",
        "https://coding.dashscope.aliyuncs.com/v1",
        "https://coding-intl.dashscope.aliyuncs.com/v1",
        "https://coding-intl.dashscope.aliyuncs.com/apps/anthropic",
        "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
    ] {
        assert!(
            qwen.contains(host),
            "the Qwen section must list the exact current upstream host {host} (FR-7.2)"
        );
    }
}

#[test]
fn providers_doc_documents_paths_and_precedence() {
    let qwen = qwen_section();
    for needle in [
        "settings.json",
        "~/.qwen",
        "QWEN_HOME",
        "QWEN_RUNTIME_DIR",
        "runtimeOutputDir",
        "usage/token-usage-",
        "usage_record.jsonl",
    ] {
        assert!(
            qwen.contains(needle),
            "the Qwen section must document {needle} (FR-1/FR-2/FR-7.2)"
        );
    }
    assert!(
        qwen.contains("anchored") || qwen.contains("anchors"),
        "relative runtimeOutputDir anchoring under the Qwen home must be documented (FR-2/FR-7.2)"
    );
    assert!(
        qwen.contains(">"),
        "the Qwen section must state the home/runtime precedence chain (FR-1/FR-7.2)"
    );
}

#[test]
fn providers_doc_documents_request_and_legacy_usage_semantics() {
    let qwen = qwen_section();
    assert!(
        qwen.contains("token-usage-YYYY-MM") || qwen.contains("local month"),
        "the request ledger's writer-local month filenames must be documented (FR-3/FR-7.2)"
    );
    assert!(
        qwen.contains("RFC3339") || qwen.contains("RFC 3339"),
        "request-ledger timestamps are RFC3339 and must be documented (FR-3/FR-7.2)"
    );
    assert!(
        qwen.contains("last"),
        "legacy last-valid-record-wins must be documented (FR-4/FR-7.2)"
    );
    assert!(
        qwen.contains("suppress") || qwen.contains("skip"),
        "request-ledger sessions suppressing the legacy summary must be documented (FR-4/FR-7.2)"
    );
    assert!(
        qwen.contains("additive") && qwen.contains("Claude Code"),
        "additive routed Claude Code qwen rows must be documented (FR-6.1/FR-7.2)"
    );
    assert!(
        qwen.contains("dedup"),
        "no-cross-client-dedup must be documented (FR-6.1/FR-7.2)"
    );
}

#[test]
fn providers_doc_states_the_trust_boundary() {
    let qwen = qwen_section();
    for needle in [
        "cookie",
        "sec_token",
        "/data/api.json",
        "account-reporting API",
        "no network",
        "price",
    ] {
        assert!(
            qwen.to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase()),
            "the Qwen section must state the {needle} trust boundary (FR-7.2)"
        );
    }
    assert!(
        qwen.contains("key-only") || qwen.contains("configured but"),
        "key-only configuration semantics must be documented (FR-5/FR-7.2)"
    );
}

#[test]
fn providers_doc_sk_sp_prefix_never_identifies_a_plan_class() {
    let qwen = qwen_section();
    assert!(
        qwen.contains("sk-sp-"),
        "the Qwen section must name the sk-sp-* prefix (FR-7.2)"
    );
    assert!(
        qwen.contains("never identifies")
            || qwen.contains("does not identify")
            || qwen.contains("never identify"),
        "the Qwen section must state the sk-sp-* prefix never identifies the plan class (FR-1/FR-7.2)"
    );
}

// ---------------------------------------------------------------------------
// Config::sample(): commented [qwen] section with fields, env names, and
// precedence (FR-7.3).
// ---------------------------------------------------------------------------

#[test]
fn config_sample_documents_the_qwen_section() {
    let sample = sample_body();
    assert!(
        sample.contains("[qwen]"),
        "Config::sample must contain a [qwen] section (FR-7.3)"
    );
    for needle in [
        "standard_key",
        "coding_plan_key",
        "token_plan_key",
        "home",
        "runtime_dir",
        "DASHSCOPE_API_KEY",
        "BAILIAN_CODING_PLAN_API_KEY",
        "QWEN_HOME",
        "QWEN_RUNTIME_DIR",
        "runtimeOutputDir",
    ] {
        assert!(
            sample.contains(needle),
            "Config::sample must document {needle} (FR-7.3)"
        );
    }
    assert!(
        sample.contains("~/.qwen"),
        "Config::sample must name the default Qwen home ~/.qwen (FR-7.3)"
    );
    assert!(
        sample.contains("console-only") || sample.contains("local"),
        "Config::sample must state Qwen usage is local Qwen Code records (FR-7.3)"
    );
}

// ---------------------------------------------------------------------------
// discover.rs module docs: Qwen source list (FR-2, FR-7.4).
// ---------------------------------------------------------------------------

#[test]
fn discover_docs_list_qwen_sources() {
    let d = read("src/discover.rs");
    let head: String = d.lines().take(40).collect::<Vec<_>>().join("\n");
    assert!(
        head.contains("Qwen") || head.contains("qwen"),
        "the discovery module docs must list Qwen sources (FR-7.4)"
    );
    for needle in [
        "settings.json",
        "QWEN_HOME",
        "QWEN_RUNTIME_DIR",
        "read-only",
    ] {
        assert!(
            head.contains(needle),
            "the discovery module docs must mention {needle} (FR-7.4)"
        );
    }
}

// ---------------------------------------------------------------------------
// clap help: --provider enumeration includes qwen (FR-6, AC-7; regression).
// ---------------------------------------------------------------------------

#[test]
fn cli_help_enumerates_qwen() {
    let m = read("src/main.rs");
    assert!(
        m.contains("(anthropic,openai,deepseek,kimi,glm,gemini,qwen)"),
        "the --provider help must enumerate qwen (FR-6)"
    );
}

// ---------------------------------------------------------------------------
// Panel-count honesty: a key-only Qwen setup is shown as configured without
// claiming live account usage (FR-5/FR-6, AC-6).
// ---------------------------------------------------------------------------

#[test]
fn key_only_qwen_is_labeled_in_the_usage_scope_line() {
    let m = read("src/main.rs");
    assert!(
        m.contains("qwen (no local usage records yet)"),
        "usage_scope_line must explain a configured Qwen provider with no local records (AC-6)"
    );
}

// ---------------------------------------------------------------------------
// CHANGELOG: release-section coverage (FR-7).
// ---------------------------------------------------------------------------

#[test]
fn changelog_release_section_covers_qwen() {
    let c = read("CHANGELOG.md");
    // Before release, these notes live under Unreleased. The release finalizer
    // moves that same body into a dated version section, so locate the section
    // by a Qwen-specific key rather than pinning mutable lifecycle state.
    let notes = c
        .split("\n## [")
        .find(|section| section.contains("BAILIAN_CODING_PLAN_API_KEY"))
        .expect("one changelog section must contain the Qwen release notes");
    for needle in [
        "Qwen",
        "DASHSCOPE_API_KEY",
        "BAILIAN_CODING_PLAN_API_KEY",
        "BAILIAN_TOKEN_PLAN_API_KEY",
        "token-usage-",
        "usage_record.jsonl",
        "console-only",
        "additive",
    ] {
        assert!(
            notes.contains(needle),
            "one CHANGELOG release section must cover {needle} (FR-7)"
        );
    }
}

// ---------------------------------------------------------------------------
// No realistic secrets in documentation examples (NFR Security, AC-9).
// ---------------------------------------------------------------------------

#[test]
fn docs_and_sample_examples_contain_no_realistic_qwen_keys() {
    let qwen = qwen_section();
    assert!(
        !has_realistic_key(&qwen),
        "docs/providers.md Qwen examples must use empty/commented values or obvious placeholders, never realistic key strings"
    );
    let sample = sample_body();
    assert!(
        !has_realistic_key(&sample),
        "Config::sample must never contain a realistic key string"
    );
    let readme = read("README.md");
    assert!(
        !has_realistic_key(&readme),
        "README must never contain a realistic key string"
    );
    let changelog = read("CHANGELOG.md");
    assert!(
        !has_realistic_key(&changelog),
        "CHANGELOG must never contain a realistic key string"
    );
}

// ---------------------------------------------------------------------------
// Self-check: this contract never imports the crate under test.
// ---------------------------------------------------------------------------

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/qwen_docs.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/qwen_docs.rs is a std-only contract and must not import llmu"
    );
}
