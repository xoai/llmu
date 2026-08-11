//! Task 3: failing quota provenance source contract (RED).
//!
//! Std-only source contract, never importing private `llmu` modules — it
//! pins the AD-1 / AD-4 / FR-3.10 surface textually (mirroring the
//! `tests/http_cache.rs` pattern) so Task 4 (Gemini) and Task 7 (cache
//! wiring) cannot drift the shape they depend on. It fails because the
//! contract does not exist yet: no `QuotaFetch` on `Provider::quotas`, no
//! provenance-aware `gather` absorption, no per-override adaptation.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

#[test]
fn providers_module_declares_quota_fetch_contract() {
    let m = read("src/providers/mod.rs");
    for needle in [
        "pub struct QuotaFetch",
        "pub snapshots: Vec<QuotaSnapshot>",
        "pub notes: Vec<String>",
        "pub refresh_last_known_good: bool",
        // Task 7 threaded the typed fetch context into every Provider
        // method; the quota override now carries it too.
        "fn quotas(&self, _cfg: &Config, _ctx: &FetchContext)",
        "fn live(",
    ] {
        assert!(
            m.contains(needle),
            "src/providers/mod.rs must declare `{needle}` (AD-4 QuotaFetch contract)"
        );
    }
}

#[test]
fn gather_queues_cache_only_for_live_nonempty_rows() {
    let main = read("src/main.rs");
    for needle in [
        "fn absorb_quota",
        "refresh_last_known_good",
        "to_cache.push",
    ] {
        assert!(
            main.contains(needle),
            "src/main.rs must `{needle}` (FR-3.10: cached-origin rows never re-age last-known-good)"
        );
    }
}

#[test]
fn gather_failure_fallback_to_cached_quotas_is_retained() {
    let main = read("src/main.rs");
    for needle in ["store::cached_quotas", "— showing cached meters"] {
        assert!(
            main.contains(needle),
            "src/main.rs must keep `{needle}` (existing error fallback, AD-1)"
        );
    }
}

#[test]
fn every_existing_quota_override_adapts_to_quota_fetch() {
    for f in ["claude_sub.rs", "codex.rs", "kimi.rs", "glm.rs"] {
        let src = read(&format!("src/providers/{f}"));
        assert!(
            src.contains("Result<QuotaFetch>"),
            "src/providers/{f} must return `Result<QuotaFetch>` from its quotas override"
        );
    }
}

#[test]
fn gemini_overrides_quotas_with_quota_fetch() {
    // Flipped by Task 4: Gemini's first quotas override landed, carrying
    // the same QuotaFetch contract every other provider adapted to in
    // Task 3.
    let gem = read("src/providers/gemini.rs");
    assert!(
        gem.contains("fn quotas(") && gem.contains("Result<QuotaFetch>"),
        "src/providers/gemini.rs must override quotas with `Result<QuotaFetch>` (Task 4, AD-4)"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/quota_provenance.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/quota_provenance.rs is a std-only contract and must not import llmu"
    );
}
