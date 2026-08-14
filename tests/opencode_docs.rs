//! Task 8: OpenCode documentation and source-inventory contracts (RED).
//!
//! Std-only, never importing private `llmu` modules — the established
//! `tests/*.rs` pattern (files on disk only: no network, no real home, no
//! process-env mutation). These contracts pin USER-VISIBLE OpenCode
//! documentation facts (DOC-1-DOC-4, AC-9): the README zero-config source
//! table and data-availability matrix row, the additive-overlap wording,
//! the ktok/hour sparkline wording, the Qwen-through-OpenCode reproduction,
//! the dedicated OpenCode local-usage section in `docs/providers.md`
//! (path precedence, exact 17-alias allowlist, strict eligibility, token
//! and cost semantics, standalone status, trust boundary, bounded
//! diagnostics), the CHANGELOG OpenCode release-note coverage (located by
//! content marker so it survives the release finalizer), the complete tracked
//! Markdown inventory (via `git ls-files`, with AGENTS.md classified as
//! generated and non-user-facing), the inventoried `src/config.rs` sample
//! and `src/main.rs` help/status wording
//! that already need no edits, and the no-overclaim / no-realistic-secret
//! rules.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

/// Body of one `## `-headed section: everything up to the next heading.
/// Matches `### ` headings too (they contain the `## ` delimiter), so the
/// extracted sections must stay prose/table-only without `###` subheads.
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

/// Owned body of the `## OpenCode local usage` section of docs/providers.md.
fn opencode_section() -> String {
    section(&read("docs/providers.md"), "## OpenCode local usage").to_string()
}

/// Body of the changelog section holding the OpenCode release notes.
/// Located by a content marker across the WHOLE changelog, never by the
/// `## [Unreleased]` heading: the release finalizer moves that same body
/// into a dated version section, so a heading-pinned contract turns every
/// generated release PR red (prior correction: never pin mutable release
/// lifecycle state). Passes both before finalization (Unreleased) and
/// after (the dated release section).
fn opencode_changelog_notes() -> String {
    read("CHANGELOG.md")
        .split("\n## [")
        .find(|section| section.contains("OpenCode local usage"))
        .expect("one changelog section must contain the OpenCode release notes")
        .to_string()
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

/// Every TRACKED Markdown file in the repository, relative to the repo
/// root, as reported by git itself. Ignored workspace tooling —
/// `.opencode/`, `.sage/`, `sage/`, `target/`, and generated process
/// files — is never tracked, so it can never pollute the user-facing
/// documentation inventory (DOC-4). Git is already required by the
/// project's release tooling.
fn markdown_inventory() -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "--", "*.md"])
        .current_dir(repo_root())
        .output()
        .expect("running git ls-files");
    assert!(
        out.status.success(),
        "git ls-files must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut files: Vec<String> = String::from_utf8(out.stdout)
        .expect("git ls-files output must be UTF-8")
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .filter(|line| !line.is_empty())
        .collect();
    files.sort();
    files
}

// ---------------------------------------------------------------------------
// Complete Markdown inventory (prior correction; DOC-4).
// ---------------------------------------------------------------------------

#[test]
fn markdown_inventory_is_exactly_the_expected_docs() {
    let found = markdown_inventory();
    for expected in ["README.md", "CHANGELOG.md", "docs/providers.md"] {
        assert!(
            found.contains(&expected.to_string()),
            "expected doc {expected} is missing from the inventory: {found:?}"
        );
    }
    let unexpected: Vec<&String> = found
        .iter()
        .filter(|f| {
            f.as_str() != "README.md"
                && f.as_str() != "CHANGELOG.md"
                && f.as_str() != "docs/providers.md"
                && f.as_str() != "AGENTS.md"
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "every Markdown file must be classified; unclassified: {unexpected:?}"
    );
}

#[test]
fn agents_md_is_generated_non_user_facing_and_unmodified() {
    let gitignore = read(".gitignore");
    assert!(
        gitignore.contains("AGENTS.md"),
        "AGENTS.md must be gitignored (generated process instruction, not repository docs)"
    );
    let readme = read("README.md");
    assert!(
        !readme.contains("AGENTS.md"),
        "the user-facing README must not reference the generated AGENTS.md"
    );
    let agents = repo_root().join("AGENTS.md");
    if !agents.exists() {
        return; // absent in task clones; the classification rules still hold
    }
    let text = fs::read_to_string(&agents).expect("reading AGENTS.md");
    assert!(
        text.to_ascii_lowercase().contains("generated"),
        "AGENTS.md must carry a generated-process-instruction marker"
    );
    let tracked = Command::new("git")
        .args(["ls-files", "--", "AGENTS.md"])
        .current_dir(repo_root())
        .output()
        .expect("running git ls-files");
    assert!(
        String::from_utf8_lossy(&tracked.stdout).trim().is_empty(),
        "AGENTS.md must not be tracked by git (unmodified generated file)"
    );
}

// ---------------------------------------------------------------------------
// README: zero-config sources, data matrix, additive overlap, sparkline,
// Qwen-through-OpenCode reproduction (DOC-1, AC-9).
// ---------------------------------------------------------------------------

#[test]
fn readme_zero_config_lists_opencode_db_usage_source() {
    let zero = readme_section("## Zero config");
    for needle in [
        "opencode.db",
        "OPENCODE_DATA_DIR",
        "~/.local/share/opencode",
        "XDG_DATA_HOME",
    ] {
        assert!(
            zero.contains(needle),
            "the zero-config table must name {needle} (DOC-1)"
        );
    }
    let row = zero
        .lines()
        .find(|l| l.contains("opencode.db"))
        .unwrap_or_else(|| panic!("the zero-config table must list an opencode.db row (DOC-1)"));
    for prov in ["qwen", "glm", "deepseek", "kimi", "openai"] {
        assert!(
            row.contains(prov),
            "the opencode.db zero-config row must cover the mapped provider {prov} (DOC-1): {row}"
        );
    }
}

#[test]
fn readme_data_matrix_has_an_opencode_row() {
    let matrix = readme_section("## The honest data-availability matrix");
    let row = matrix
        .lines()
        .find(|l| l.trim_start().starts_with("| OpenCode"))
        .unwrap_or_else(|| {
            panic!("the data matrix must include an OpenCode row (DOC-1): {matrix}")
        });
    assert!(
        row.contains("opencode.db"),
        "the OpenCode matrix row must name the local database (DOC-1): {row}"
    );
    for prov in ["qwen", "glm", "deepseek", "kimi", "openai"] {
        assert!(
            row.contains(prov),
            "the OpenCode matrix row must cover the mapped provider {prov} (DOC-1): {row}"
        );
    }
    assert!(
        row.contains("additive"),
        "the OpenCode matrix row must state records are additive with APIs/client logs (DOC-1): {row}"
    );
    assert!(
        !row.contains("Cost API"),
        "the OpenCode matrix row must not claim a billed-cost API (DOC-1): {row}"
    );
}

#[test]
fn readme_matrix_preserves_existing_provider_rows() {
    let matrix = readme_section("## The honest data-availability matrix");
    let qwen = matrix
        .lines()
        .find(|l| l.trim_start().starts_with("| Qwen"))
        .unwrap_or_else(|| panic!("the existing Qwen matrix row must remain (DOC-1)"));
    assert!(
        qwen.contains("Qwen Code") && qwen.contains("local"),
        "the existing Qwen row must retain its local Qwen Code meaning (DOC-1): {qwen}"
    );
    let deepseek = matrix
        .lines()
        .find(|l| l.trim_start().starts_with("| DeepSeek"))
        .unwrap_or_else(|| panic!("the existing DeepSeek matrix row must remain (DOC-1)"));
    assert!(
        deepseek.contains("balance"),
        "the existing DeepSeek row must retain its balance-only meaning (DOC-1): {deepseek}"
    );
}

#[test]
fn readme_states_local_additive_overlap_without_dedup_claim() {
    let sec = readme_section("## OpenCode local usage records");
    assert!(
        sec.contains("additive"),
        "the README OpenCode section must state records are additive (DOC-1)"
    );
    assert!(
        sec.contains("overlap"),
        "the README OpenCode section must acknowledge possible overlap (DOC-1)"
    );
    assert!(
        sec.contains("never deduplicat") || sec.contains("no request dedup"),
        "the README OpenCode section must explicitly disclaim request dedup (DOC-1)"
    );
    assert!(
        sec.contains("estimate") && sec.contains("never"),
        "the README OpenCode section must say only llmu pricing estimates appear, never billed cost (DOC-1)"
    );
    assert!(
        !sec.contains("quota"),
        "the README OpenCode section must not claim a quota feed (DOC-1)"
    );
}

#[test]
fn readme_documents_qwen_through_opencode_reproduction() {
    let sec = readme_section("## OpenCode local usage records");
    assert!(
        sec.contains("llmu providers"),
        "the reproduction must start from llmu providers (DOC-1)"
    );
    assert!(
        sec.contains("--provider qwen") && sec.contains("llmu usage"),
        "the reproduction must run llmu usage --provider qwen (DOC-1)"
    );
    assert!(
        sec.contains("--model"),
        "the reproduction must show the model substring filter (DOC-1)"
    );
    assert!(
        sec.contains("--source") && sec.contains("local"),
        "the reproduction must show the source filter (DOC-1)"
    );
}

#[test]
fn readme_live_watch_sparkline_is_ktok_per_hour() {
    let watch = readme_section("## Live watch mode");
    assert!(
        watch.contains("ktok/hour"),
        "the live sparkline wording must be ktok/hour, not tokens/hour (T5 retained wording)"
    );
    assert!(
        !watch.contains("tokens/hour"),
        "the live watch section must not claim tokens/hour (T5 retained wording)"
    );
}

#[test]
fn readme_panel_counts_include_opencode_records() {
    let panels = readme_section("## What each panel counts");
    assert!(
        panels.contains("OpenCode"),
        "panel-count docs must list local OpenCode records among the usage feeds (DOC-1)"
    );
}

// ---------------------------------------------------------------------------
// docs/providers.md: dedicated OpenCode local-usage section (DOC-2).
// ---------------------------------------------------------------------------

#[test]
fn providers_doc_has_an_opencode_local_usage_section() {
    let sec = opencode_section();
    assert!(
        sec.contains("implemented") && sec.contains("local"),
        "the OpenCode section must state its implemented local status (DOC-2)"
    );
    assert!(
        sec.contains("message"),
        "the OpenCode section must name the message-table source (DOC-2)"
    );
}

#[test]
fn providers_doc_documents_path_precedence_and_shared_resolver() {
    let sec = opencode_section();
    for needle in [
        "OPENCODE_DATA_DIR",
        "XDG_DATA_HOME",
        "~/.local/share/opencode",
        "opencode.db",
    ] {
        assert!(
            sec.contains(needle),
            "the OpenCode section must document {needle} (DOC-2)"
        );
    }
    assert!(
        sec.contains("shared resolver"),
        "the OpenCode section must state auth and database share one resolver (DOC-2)"
    );
    assert!(
        sec.contains("auth.json") && sec.contains("separate"),
        "the OpenCode section must keep auth discovery distinct from usage (DOC-2)"
    );
}

#[test]
fn providers_doc_lists_the_exact_provider_alias_allowlist() {
    let sec = opencode_section();
    for alias in [
        "alibaba",
        "alibaba-cn",
        "alibaba-coding-plan",
        "alibaba-coding-plan-cn",
        "alibaba-token-plan",
        "alibaba-token-plan-cn",
        "bailian-token-plan-personal",
        "zai",
        "zai-coding-plan",
        "zhipuai",
        "zhipuai-coding-plan",
        "deepseek",
        "kimi-for-coding",
        "moonshot",
        "moonshotai",
        "kimi",
        "openai",
    ] {
        assert!(
            sec.contains(alias),
            "the OpenCode section must list the exact alias {alias} (DOC-2)"
        );
    }
    assert!(
        sec.contains("case-sensitive") && sec.contains("exact"),
        "the OpenCode section must state attribution is exact and case-sensitive (DOC-2)"
    );
    for prov in ["qwen", "glm", "deepseek", "kimi", "openai"] {
        assert!(
            sec.contains(prov),
            "the OpenCode section must state the canonical provider {prov} (DOC-2)"
        );
    }
}

#[test]
fn providers_doc_skips_unknown_and_generic_ids_without_model_inference() {
    let sec = opencode_section();
    assert!(
        sec.to_ascii_lowercase().contains("unknown"),
        "the OpenCode section must state unknown provider IDs are skipped (DOC-2)"
    );
    assert!(
        sec.contains("generic") || sec.contains("`opencode`"),
        "the OpenCode section must name the generic opencode provider id (DOC-2)"
    );
    assert!(
        sec.contains("model-prefix") || sec.contains("model prefix"),
        "the OpenCode section must state model-prefix inference never happens (DOC-2)"
    );
    assert!(
        sec.contains("never"),
        "the OpenCode section must phrase the inference prohibition with never (DOC-2)"
    );
}

#[test]
fn providers_doc_documents_strict_eligibility() {
    let sec = opencode_section();
    for needle in [
        "assistant",
        "tool-calls",
        "stop",
        "length",
        "error",
        "nonnegative",
        "positive",
        "malformed",
    ] {
        assert!(
            sec.contains(needle),
            "the OpenCode section must document the {needle} eligibility rule (DOC-2)"
        );
    }
    assert!(
        sec.contains("duplicate"),
        "the OpenCode section must state duplicate decoded keys are rejected (DOC-2)"
    );
}

#[test]
fn providers_doc_documents_token_and_cost_semantics() {
    let sec = opencode_section();
    for needle in ["saturating", "u64::MAX", "hourly", "reasoning", "cache"] {
        assert!(
            sec.contains(needle),
            "the OpenCode section must document {needle} semantics (DOC-2)"
        );
    }
    assert!(
        sec.contains("cache-subtracted") || sec.contains("subtract"),
        "the OpenCode section must state input is never cache-subtracted (DOC-2)"
    );
    assert!(
        sec.contains("ignored") && sec.contains("[pricing]") && sec.contains("estimate"),
        "the OpenCode section must say OpenCode's local cost is ignored and only llmu estimates appear (DOC-2)"
    );
    assert!(
        sec.contains("never") && sec.contains("billed"),
        "the OpenCode section must exclude provider-billed/account cost (DOC-2)"
    );
}

#[test]
fn providers_doc_documents_additive_overlap_and_filters() {
    let sec = opencode_section();
    assert!(
        sec.contains("additive") && sec.contains("overlap"),
        "the OpenCode section must state additive accounting with possible overlap (DOC-2)"
    );
    assert!(
        sec.contains("deduplicat"),
        "the OpenCode section must state no cross-source deduplication (DOC-2)"
    );
    for needle in ["--provider", "--model", "--source"] {
        assert!(
            sec.contains(needle),
            "the OpenCode section must document the {needle} filter (DOC-2)"
        );
    }
    assert!(
        sec.contains("--provider qwen"),
        "the OpenCode section must say a qwen filter admits every mapped Qwen alias (DOC-2)"
    );
}

#[test]
fn providers_doc_documents_standalone_status() {
    let sec = opencode_section();
    for needle in [
        "llmu providers",
        "one-row",
        "same strict validator",
        "lighter than collection",
        "not constant-time",
        "may examine every",
        "no diagnostics",
    ] {
        assert!(
            sec.contains(needle),
            "the OpenCode section must document the status behavior '{needle}' (DOC-2)"
        );
    }
}

#[test]
fn providers_doc_documents_the_trust_boundary() {
    let sec = opencode_section();
    for needle in [
        "read-only",
        "query_only",
        "250 ms",
        "WAL",
        "immutable",
        "snapshot",
        "migrat",
        "vacuum",
        "event",
        "part",
        "cookie",
        "console",
        "network",
    ] {
        assert!(
            sec.contains(needle),
            "the OpenCode section must state the {needle} trust boundary (DOC-2)"
        );
    }
    assert!(
        sec.contains("never"),
        "the OpenCode section must phrase the no-write boundary with never (DOC-2)"
    );
    assert!(
        sec.contains("paths") && sec.contains("SQL"),
        "the OpenCode section must state diagnostics never leak paths or SQL (DOC-2)"
    );
}

#[test]
fn providers_doc_documents_bounded_diagnostics() {
    let sec = opencode_section();
    for needle in [
        "malformed",
        "unsupported provider",
        "busy or unreadable",
        "unsupported schema",
        "missing database is silent",
    ] {
        assert!(
            sec.contains(needle),
            "the OpenCode section must document the {needle} diagnostic (DOC-2)"
        );
    }
}

#[test]
fn providers_doc_cross_references_provider_sections() {
    let sec = opencode_section();
    for prov in ["Qwen", "GLM", "DeepSeek", "Kimi", "OpenAI"] {
        assert!(
            sec.contains(prov),
            "the OpenCode section must cross-reference the {prov} section instead of duplicating it (DOC-2)"
        );
    }
}

#[test]
fn provider_sections_cross_reference_the_opencode_section() {
    let doc = read("docs/providers.md");
    for heading in ["## Qwen", "## GLM", "## DeepSeek", "## Kimi", "## OpenAI"] {
        let s = section(&doc, heading);
        assert!(
            s.contains("OpenCode local usage"),
            "the {heading} section must cross-reference the shared OpenCode local usage section (DOC-2)"
        );
    }
}

// ---------------------------------------------------------------------------
// CHANGELOG: OpenCode release-note coverage (DOC-3), located by content
// marker so the contract survives the release finalizer moving the same
// body from Unreleased into the dated release section.
// ---------------------------------------------------------------------------

#[test]
fn changelog_unreleased_covers_opencode() {
    let notes = opencode_changelog_notes();
    for needle in [
        "OpenCode",
        "Qwen",
        "GLM",
        "Kimi",
        "OpenAI",
        "read-only",
        "WAL",
        "additive",
        "saturat",
        "allowlist",
    ] {
        assert!(
            notes.contains(needle),
            "the CHANGELOG Unreleased notes must cover {needle} (DOC-3)"
        );
    }
    assert!(
        notes.contains("providers"),
        "the CHANGELOG must name the standalone opencode status row (DOC-3)"
    );
    assert!(
        notes.contains("observed"),
        "the CHANGELOG must name the observed Qwen gap being closed (DOC-3)"
    );
    assert!(
        !notes.contains("quota"),
        "the CHANGELOG must not overclaim a quota source (DOC-3)"
    );
    assert!(
        notes.contains("OpenCode local usage"),
        "the located section must be the OpenCode release notes (DOC-3)"
    );
}

// ---------------------------------------------------------------------------
// Source inventory: src/config.rs sample and src/main.rs help/status were
// inventoried and already need no edits (DOC-4).
// ---------------------------------------------------------------------------

#[test]
fn config_sample_inventoried_needs_no_opencode_section() {
    let sample = sample_body();
    assert!(
        sample.contains("OpenCode"),
        "the generated sample's discovery note must already name OpenCode auth discovery (inventoried, credential context)"
    );
    assert!(
        !sample.contains("[opencode]"),
        "no [opencode] section belongs in the sample: usage is zero-config auto-discovery with no new keys (DOC-4)"
    );
}

#[test]
fn cli_status_and_help_inventoried_already_correct() {
    let m = read("src/main.rs");
    assert!(
        m.contains("read-only local usage from OpenCode's SQLite message records across supported providers"),
        "the approved opencode providers capability wording must remain in src/main.rs (inventoried, no edit needed)"
    );
    assert!(
        m.contains("(anthropic,openai,deepseek,kimi,glm,gemini,qwen)"),
        "the --provider help enumeration must be unchanged (opencode events surface under canonical ids, DOC-4)"
    );
    assert!(
        !m.contains("(anthropic,openai,deepseek,kimi,glm,gemini,qwen,opencode)"),
        "opencode must not become a filterable --provider id (DOC-4)"
    );
}

// ---------------------------------------------------------------------------
// No forbidden overclaims anywhere in the OpenCode docs (AC-9).
// ---------------------------------------------------------------------------

#[test]
fn opencode_docs_make_no_forbidden_claims() {
    let combined = format!(
        "{}\n{}\n{}",
        opencode_section(),
        readme_section("## OpenCode local usage records"),
        opencode_changelog_notes()
    );
    for bad in [
        "deduplicated",
        "scrapes",
        "writes to the database",
        "mutates",
        "cost is used",
    ] {
        assert!(
            !combined.contains(bad),
            "forbidden overclaim `{bad}` must not appear in the OpenCode docs (AC-9)"
        );
    }
}

// ---------------------------------------------------------------------------
// No realistic credentials or developer-machine paths (NFR Security, AC-9).
// ---------------------------------------------------------------------------

#[test]
fn docs_contain_no_realistic_keys_or_machine_paths() {
    for f in ["README.md", "CHANGELOG.md", "docs/providers.md"] {
        let text = read(f);
        assert!(
            !has_realistic_key(&text),
            "{f} must never contain a realistic key string"
        );
        assert!(
            !text.contains("/home/"),
            "{f} must not contain a developer-machine absolute path"
        );
    }
}

// ---------------------------------------------------------------------------
// Self-check: this contract never imports the crate under test.
// ---------------------------------------------------------------------------

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/opencode_docs.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/opencode_docs.rs is a std-only contract and must not import llmu"
    );
}
