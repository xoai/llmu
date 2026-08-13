//! Task 2: read-only local Qwen usage provider contracts (RED -> GREEN).
//!
//! Std-only, never importing private `llmu` modules. Mirrors the
//! `tests/qwen_config.rs` pattern: source contracts pin the FR-3 through
//! FR-6 / AC-3 through AC-6 / AC-8 / AC-11 surface textually, and the
//! registration contract drives the real binary (`CARGO_BIN_EXE_llmu`)
//! against isolated HOME/XDG directories with every provider env var
//! removed, so no real credential, real-home Qwen discovery, or network
//! ever participates.
//!
//! The source contract scans all Qwen-touched production sources
//! (`src/providers/qwen.rs`, `src/config.rs`, `src/discover.rs`) for
//! HTTP helpers, console-session markers (`sec_token`, cookies,
//! `/data/api.json`), and proves `Cargo.toml` gains no dependency
//! (AC-8, AC-11). `src/providers/mod.rs` is deliberately not scanned
//! for markers (user disposition on plan finding F-029); it is only
//! checked for the registration lines below.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

// ---------------------------------------------------------------------------
// Registration (FR-6, AC-7): provider id `qwen` must be listed by
// `llmu providers` and registered in `providers::all()`.
// ---------------------------------------------------------------------------

#[test]
fn providers_table_lists_qwen_when_registered() {
    let sb = Sandbox::new("registered");
    let out = sb.run(&["providers"]);
    assert!(
        out.status.success(),
        "llmu providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    assert!(
        stdout.contains("qwen"),
        "llmu providers must list the qwen provider row (FR-6/AC-7):\n{stdout}"
    );
    assert!(
        stdout.contains("capabilities"),
        "the providers table must still render (FR-6):\n{stdout}"
    );
}

#[test]
fn provider_registration_source_registers_qwen() {
    let m = read("src/providers/mod.rs");
    assert!(
        m.contains("pub mod qwen;"),
        "src/providers/mod.rs must declare `pub mod qwen;` (FR-6)"
    );
    assert!(
        m.contains("qwen::Qwen"),
        "src/providers/mod.rs must register `qwen::Qwen` in providers::all() (FR-6)"
    );
}

// ---------------------------------------------------------------------------
// Provider surface (FR-3 through FR-5): qwen.rs implements the local
// request-ledger + legacy parser behind the Provider trait.
// ---------------------------------------------------------------------------

#[test]
fn qwen_provider_source_declares_usage_configured_and_capabilities() {
    let src = read("src/providers/qwen.rs");
    for needle in [
        "pub struct Qwen",
        "fn usage(",
        "fn configured(",
        "fn capabilities(",
        "schemaVersion",
        "token-usage-",
        "usage_record.jsonl",
        "saturating_sub",
        "saturating_add",
    ] {
        assert!(
            src.contains(needle),
            "src/providers/qwen.rs must `{needle}` (FR-3/FR-4/FR-6)"
        );
    }
}

// ---------------------------------------------------------------------------
// Console/network exclusions (AC-8): no HTTP helper, cookie, sec_token,
// or /data/api.json console-reporting marker in any Qwen source.
// ---------------------------------------------------------------------------

#[test]
fn qwen_sources_contain_no_http_or_console_session_markers() {
    for rel in ["src/providers/qwen.rs", "src/config.rs", "src/discover.rs"] {
        let src = read(rel);
        for needle in [
            "use crate::http",
            "http::",
            "get_json_cached",
            "post_json",
            "post_form_json",
            "ureq",
            "reqwest",
            "sec_token",
            "/data/api.json",
        ] {
            assert!(
                !src.contains(needle),
                "{rel} must not contain `{needle}` (AC-8 no console-session transport)"
            );
        }
        let lower = src.to_ascii_lowercase();
        assert!(
            !lower.contains("cookie"),
            "{rel} must not discover or store browser cookies (AC-8)"
        );
    }
}

// ---------------------------------------------------------------------------
// No new dependency (AC-11): the `[dependencies]` section is snapshotted
// byte-for-byte; any future Qwen (or other) dependency change must update
// this contract deliberately.
// ---------------------------------------------------------------------------

#[test]
fn cargo_toml_dependencies_unchanged() {
    let cargo = read("Cargo.toml");
    let snapshot = "[dependencies]\n\
                    anyhow = \"1.0\"\n\
                    chrono = { version = \"0.4.38\", features = [\"serde\"] }\n\
                    clap = { version = \"=4.5.4\", features = [\"derive\"] }\n\
                    crossterm = \"=0.27.0\"\n\
                    dirs = \"=5.0.1\"\n\
                    ratatui = \"=0.26.3\"\n\
                    # pin: bundled SQLite for read-only OpenCode usage (FR-4, NFR-12) — exact\n\
                    # 0.31.0 compiles on rust-version 1.75; no system sqlite dependency.\n\
                    # `functions` adds no transitive dependency (status scalar, FR-30).\n\
                    rusqlite = { version = \"=0.31.0\", features = [\"bundled\", \"functions\"] }\n\
                    serde = { version = \"1.0\", features = [\"derive\"] }\n\
                    serde_json = \"1.0\"\n\
                    sha2 = \"=0.10.9\"\n\
                    toml = \"=0.8.14\"\n\
                    ureq = { version = \"=2.9.7\", features = [\"json\"] }\n\
                    # pin: url 2.5.1+ pulls ICU idna stack that needs newer rustc\n\
                    url = \"=2.5.0\"";
    assert!(
        cargo.contains(snapshot),
        "Cargo.toml [dependencies] must stay unchanged (AC-11)"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/qwen_provider.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/qwen_provider.rs is a std-only contract and must not import llmu"
    );
}

// ---------------------------------------------------------------------------
// Hermetic binary sandbox (no real home, no provider env vars, no network).
// ---------------------------------------------------------------------------

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "llmu-qwen-provider-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::create_dir_all(dir.join("data")).unwrap();
        Sandbox { dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_llmu"))
            .args(args)
            .env("HOME", self.dir.join("home"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env_remove("DASHSCOPE_API_KEY")
            .env_remove("BAILIAN_API_KEY")
            .env_remove("BAILIAN_CODING_PLAN_API_KEY")
            .env_remove("BAILIAN_TOKEN_PLAN_API_KEY")
            .env_remove("QWEN_HOME")
            .env_remove("QWEN_RUNTIME_DIR")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("ANTHROPIC_BASE_URL")
            .env_remove("OPENCODE_DATA_DIR")
            .env_remove("KIMI_SHARE_DIR")
            .env_remove("DEEPSEEK_API_KEY")
            .env_remove("MOONSHOT_API_KEY")
            .env_remove("KIMI_API_KEY")
            .env_remove("KIMI_CODE_API_KEY")
            .env_remove("ZAI_API_KEY")
            .env_remove("ZHIPU_API_KEY")
            .env_remove("ANTHROPIC_ADMIN_KEY")
            .env_remove("OPENAI_ADMIN_KEY")
            .env_remove("GEMINI_CLI_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .output()
            .expect("spawning llmu binary")
    }

    fn stdout(&self, out: &Output) -> String {
        String::from_utf8(out.stdout.clone()).expect("utf8 stdout")
    }

    fn stderr(&self, out: &Output) -> String {
        String::from_utf8(out.stderr.clone()).expect("utf8 stderr")
    }
}
