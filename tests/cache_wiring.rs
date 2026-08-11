//! Task 7: failing cache-wiring source/CLI contract (RED).
//!
//! Std-only source contract, never importing private `llmu` modules — it
//! pins the FR-3 / AS-7 / AS-8 wiring surface textually (mirroring the
//! `tests/quota_provenance.rs` and `tests/http_cache.rs` patterns) so
//! later tasks cannot drift the shape they depend on. It fails because
//! the contract does not exist yet: no global `--fresh`, no typed
//! fetch-context, no per-call-site cache opt-in, no TUI freshness state
//! machine.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

#[test]
fn cli_declares_global_fresh_and_builds_typed_fetch_context() {
    let main = read("src/main.rs");
    assert!(
        main.contains("fresh: bool"),
        "src/main.rs must declare the global `--fresh` flag (FR-3.2)"
    );
    assert!(
        main.contains("FetchContext::from_config(&cfg, cli.fresh)"),
        "one-shot commands must build the typed fetch context from config + --fresh (plan Task 7)"
    );
    assert!(
        main.contains("cli.fresh"),
        "the global --fresh value must reach every one-shot path and the TUI"
    );
}

#[test]
fn every_one_shot_gather_passes_the_typed_fetch_context() {
    let main = read("src/main.rs");
    // Only the non-test half: main.rs's own tests module legitimately
    // contains the needle in its assertions.
    let prod = main.split("#[cfg(test)]").next().unwrap();
    let call_sites: Vec<&str> = prod
        .lines()
        .filter(|l| l.contains("gather(") && !l.trim_start().starts_with("pub(crate) fn gather"))
        .collect();
    assert!(
        !call_sites.is_empty(),
        "src/main.rs must contain one-shot gather call sites"
    );
    for line in &call_sites {
        assert!(
            line.contains("ctx"),
            "every one-shot gather must pass the typed fetch context, got: {line}"
        );
    }
}

#[test]
fn providers_module_declares_typed_fetch_context() {
    let m = read("src/providers/mod.rs");
    for needle in [
        "pub struct FetchContext",
        "pub cache: http::CacheOptions",
        "pub fresh: bool",
        "_ctx: &FetchContext",
    ] {
        assert!(
            m.contains(needle),
            "src/providers/mod.rs must declare `{needle}` (plan Task 7: typed per-fetch context, no hidden global state)"
        );
    }
    for method in ["fn usage(", "fn quotas(", "fn balances("] {
        assert!(
            m.contains(method),
            "src/providers/mod.rs must keep `{method}` on the Provider trait"
        );
    }
    assert_eq!(
        m.matches("_ctx: &FetchContext").count(),
        3,
        "usage, quotas, and balances must each accept the typed fetch context"
    );
}

#[test]
fn every_eligible_provider_get_uses_the_cached_path() {
    for f in [
        "anthropic.rs",
        "claude_sub.rs",
        "codex.rs",
        "deepseek.rs",
        "glm.rs",
        "kimi.rs",
        "openai.rs",
    ] {
        let src = read(&format!("src/providers/{f}"));
        assert!(
            src.contains("get_json_cached"),
            "src/providers/{f} must opt every side-effect-free JSON GET into `get_json_cached` (FR-3.3, AS-7)"
        );
    }
}

#[test]
fn gemini_and_oauth_posts_remain_cache_ineligible() {
    let gem = read("src/providers/gemini.rs");
    assert!(
        !gem.contains("get_json_cached"),
        "Gemini is signature-only: its POST and local-file operations must remain cache-ineligible (plan Task 7)"
    );
    assert!(
        gem.contains("post_json") && gem.contains("post_form_json"),
        "Gemini's OAuth form POST and quota RPC POSTs stay uncached"
    );
    let claude = read("src/providers/claude_sub.rs");
    assert!(
        claude.contains("post_json"),
        "the Claude OAuth token POST must stay uncached (FR-3.3)"
    );
}

#[test]
fn quota_providers_wire_raw_cache_origin_into_last_known_good_marker() {
    for f in ["claude_sub.rs", "codex.rs", "glm.rs", "kimi.rs"] {
        let src = read(&format!("src/providers/{f}"));
        assert!(
            src.contains("CacheOrigin::Live"),
            "src/providers/{f} must derive `refresh_last_known_good` from the raw cache origin (FR-3.10, AS-7)"
        );
    }
}

#[test]
fn tui_carries_initial_freshness_and_a_pure_refresh_state_machine() {
    let tui = read("src/tui.rs");
    assert!(
        tui.contains("FreshState"),
        "src/tui.rs must keep the freshness bypass as a pure state machine (FR-3.2)"
    );
    assert!(
        tui.contains("FetchContext::from_config"),
        "src/tui.rs must build the typed fetch context per network tick"
    );
    let main = read("src/main.rs");
    assert!(
        main.contains("tui::run(cfg, since, refresh, local_refresh, cli.fresh)"),
        "the global --fresh must reach the TUI as its initial-fetch bypass (FR-3.2)"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/cache_wiring.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/cache_wiring.rs is a std-only contract and must not import llmu"
    );
}
