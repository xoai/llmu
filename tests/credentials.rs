//! Task 2: failing rotation-safe credential transaction contract (RED).
//!
//! Std-only source contract, never importing private `llmu` modules — it
//! pins the AD-2 / FR-4 surface textually (mirroring the
//! `tests/http_cache.rs` pattern) so Tasks 4/5 cannot drift the shape they
//! depend on. It fails because the contract does not exist yet: no
//! `mod credentials;`, no shared credential transaction API, no llmu lock.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

#[test]
fn main_wires_the_shared_credentials_module() {
    let main = read("src/main.rs");
    assert!(
        main.contains("mod credentials;"),
        "src/main.rs must declare `mod credentials;` so the shared transaction layer is compiled"
    );
}

#[test]
fn credentials_module_exposes_transaction_api() {
    let c = read("src/credentials.rs");
    for needle in [
        "pub struct CredentialSchema",
        "pub struct LockTiming",
        "pub enum ReplaceOutcome",
        "pub fn replace_with_cas",
        "pub fn read_json",
        "pub const LOCK_SUFFIX",
    ] {
        assert!(
            c.contains(needle),
            "src/credentials.rs must expose `{needle}` (AD-2 shared credential transaction)"
        );
    }
    assert!(
        c.contains(".llmu-refresh.lock"),
        "the sibling lock must carry the llmu-only suffix `<credential-filename>.llmu-refresh.lock` (FR-4.7)"
    );
}

#[test]
fn lock_protocol_is_create_new_with_pid_epoch_nonce() {
    let c = read("src/credentials.rs");
    assert!(
        c.contains("create_new(true)"),
        "the llmu lock must be acquired with OpenOptions::create_new(true) (FR-4.7)"
    );
    assert!(
        c.contains("std::process::id()"),
        "lock contents must carry the acquiring pid (FR-4.7)"
    );
    assert!(
        c.contains("epoch"),
        "lock contents must carry a creation epoch (FR-4.7)"
    );
    assert!(
        c.contains("nonce"),
        "lock contents must carry a per-attempt nonce (FR-4.7)"
    );
}

#[test]
fn default_lock_timing_matches_fr_4_7() {
    let c = read("src/credentials.rs");
    assert!(
        c.contains("Duration::from_millis(100)"),
        "lock acquisition must retry every 100ms (FR-4.7)"
    );
    assert!(
        c.contains("Duration::from_secs(5)"),
        "lock acquisition must time out at five seconds (FR-4.7)"
    );
    assert!(
        c.contains("Duration::from_secs(60)"),
        "a lock older than 60 seconds must be treated as stale (FR-4.7)"
    );
}

#[test]
fn temp_writes_are_same_directory_flushed_and_renamed() {
    let c = read("src/credentials.rs");
    assert!(
        c.contains("sync_all"),
        "the temp file must be flushed before rename (FR-4.5)"
    );
    assert!(
        c.contains("fs::rename"),
        "the temp file must be renamed over the target (FR-4.5)"
    );
    assert!(
        c.contains(".tmp"),
        "temp files must carry a unique `.tmp` suffix (FR-4.5)"
    );
    assert!(
        c.contains("create_new(true)"),
        "temp files must be created with create_new so the name is unique (FR-4.5)"
    );
}

#[test]
fn shared_module_owns_no_provider_schema() {
    let c = read("src/credentials.rs");
    for forbidden in [
        "claudeAiOauth",
        "accessToken",
        "refreshToken",
        "expiresAt",
        "oauth_creds",
        "googleapis",
        "platform.claude.com",
    ] {
        assert!(
            !c.contains(forbidden),
            "provider schema name `{forbidden}` must not live in the shared module (AD-2: provider-owned payloads)"
        );
    }
    assert!(
        c.contains("serde_json::Value"),
        "the transaction must accept parsed JSON, not provider structs (AD-2)"
    );
    assert!(
        c.contains("FnOnce"),
        "the transaction must take a provider-owned merge closure (AD-2)"
    );
}

#[test]
fn shared_module_never_logs_credentials() {
    let c = read("src/credentials.rs");
    assert!(
        !c.contains("println!") && !c.contains("eprintln!"),
        "the shared module never logs; diagnostics are error values only (NFR Security)"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/credentials.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/credentials.rs is a std-only contract and must not import llmu"
    );
}
