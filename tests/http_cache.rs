//! Task 1: failing cache-aware HTTP source/config contract (RED).
//!
//! Std-only source contract, never importing private `llmu` modules — it
//! pins the FR-3 / AD-4 surface textually (mirroring the
//! `tests/release_automation.rs` pattern) so Task 7 wiring cannot regress
//! the shape it depends on. It fails because the contract does not exist
//! yet: no `sha2` pin, no `[http_cache]` config, no cache/POST API.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

#[test]
fn cargo_toml_pins_sha2_exactly() {
    let cargo = read("Cargo.toml");
    assert!(
        cargo.contains("sha2 = \"=0.10.9\""),
        "Cargo.toml must declare `sha2 = \"=0.10.9\"` (exact pin, AD-3) for FR-3.5 cache-key hashing"
    );
}

#[test]
fn config_declares_http_cache_section_with_default_disabled_ttl() {
    let cfg = read("src/config.rs");
    assert!(
        cfg.contains("pub struct HttpCacheCfg"),
        "src/config.rs must declare HttpCacheCfg (FR-3.1 `[http_cache]`)"
    );
    assert!(
        cfg.contains("pub http_cache: HttpCacheCfg"),
        "Config must carry an `http_cache` field so existing configs stay valid (FR-7.5)"
    );
    assert!(
        cfg.contains("ttl_seconds"),
        "HttpCacheCfg must expose `ttl_seconds` (zero disables reads/writes, FR-3.1)"
    );
}

#[test]
fn http_exposes_cache_provenance_and_post_helpers() {
    let http = read("src/http.rs");
    for needle in [
        "pub enum CacheOrigin",
        "pub struct CachedJson",
        "pub struct CacheOptions",
        "pub fn get_json_cached",
        "pub fn post_json",
        "pub fn post_form_json",
    ] {
        assert!(
            http.contains(needle),
            "src/http.rs must expose `{needle}` (AD-4: cache-aware GET + uncached POST helpers)"
        );
    }
    assert!(
        http.contains("pub struct HttpStatusError"),
        "src/http.rs keeps HttpStatusError for POST status handling (AD-4)"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/http_cache.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/http_cache.rs is a std-only contract and must not import llmu"
    );
}
