//! Task 3: wire Qwen selection, routed Claude rows, and terminal identity
//! (RED -> GREEN).
//!
//! Std-only, never importing private `llmu` modules. Mirrors the
//! `tests/qwen_provider.rs` pattern: black-box tests drive the real binary
//! (`CARGO_BIN_EXE_llmu`) against an isolated HOME/XDG config with explicit
//! Qwen home/runtime paths and every provider credential/fallback env var
//! removed, so no real credential, real-home Qwen discovery, or network
//! ever participates; source contracts pin the FR-6 / FR-6.1 / AC-7 surface
//! textually.
//!
//! Fixture (fixed timestamps, no `Utc::now()` membership):
//! - one Qwen Code request-ledger row (`qwen-max`) under the explicit
//!   runtime directory;
//! - one Claude Code transcript assistant row with a `qwen-plus` model
//!   under the isolated HOME's `.claude/projects`.
//!
//! `--provider qwen` must include both clients additively; any non-Qwen
//! filter must exclude both (FR-6.1). CLI help, `UsageEvent.provider`, ANSI,
//! and TUI must enumerate/render `qwen` (FR-6).

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
// Black-box: `usage --provider qwen` includes both clients additively, and
// a non-Qwen provider filter excludes both (FR-6.1, AC-7).
// ---------------------------------------------------------------------------

/// One well-formed request-ledger line (upstream tokenUsageService.ts).
fn req_record() -> String {
    r#"{"schemaVersion":1,"id":"q-req-1","timestamp":"2026-08-10T10:00:00Z","localDate":"2026-08-10","localMonth":"2026-08","sessionId":"s-qwen","model":"qwen-max","authType":"dashscope","source":"api","inputTokens":1500,"outputTokens":800,"cachedTokens":1000,"thoughtsTokens":200,"totalTokens":2500,"apiDurationMs":1234}"#
        .to_string()
}

/// One Claude Code transcript assistant row with a qwen-prefixed model:
/// `infer_provider` attributes it to `qwen` (FR-6.1).
fn claude_transcript_row() -> String {
    r#"{"timestamp":"2026-08-11T12:00:00Z","requestId":"cc-req-1","message":{"id":"cc-msg-1","model":"qwen-plus","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":25,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"hello"}]}}"#
        .to_string()
}

#[test]
fn usage_provider_qwen_includes_both_clients_additively() {
    let sb = Sandbox::new("both-included");
    let out = sb.run(&[
        "usage",
        "--since",
        "2026-08-01",
        "--until",
        "2026-08-31",
        "--provider",
        "qwen",
        "--group-by",
        "provider,model",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "llmu usage --provider qwen must succeed: {}",
        sb.stderr(&out)
    );
    let rows = sb.json_rows(&out);

    // The Qwen Code request row (uncached 1500-1000, output 800+200).
    let qwen_max = rows
        .iter()
        .find(|r| r["keys"] == serde_json::json!(["2026-08-10", "qwen", "qwen-max"]))
        .unwrap_or_else(|| panic!("missing Qwen Code request row: {rows:?}"));
    assert_eq!(qwen_max["requests"], 1);
    assert_eq!(
        qwen_max["input_tokens"], 500,
        "uncached input = input - cached"
    );
    assert_eq!(qwen_max["output_tokens"], 1000, "output + thoughts folded");
    assert_eq!(qwen_max["cache_read_tokens"], 1000);

    // The routed Claude Code transcript row (qwen* model -> provider qwen).
    let qwen_plus = rows
        .iter()
        .find(|r| r["keys"] == serde_json::json!(["2026-08-11", "qwen", "qwen-plus"]))
        .unwrap_or_else(|| panic!("missing routed Claude Code row: {rows:?}"));
    assert_eq!(qwen_plus["requests"], 1);
    assert_eq!(qwen_plus["input_tokens"], 100);
    assert_eq!(qwen_plus["output_tokens"], 50);
    assert_eq!(qwen_plus["cache_read_tokens"], 25);

    assert_eq!(
        rows.len(),
        2,
        "one Qwen Code request + one Claude Code transcript = two additive rows, no cross-client dedup"
    );
    let reqs: u64 = rows.iter().map(|r| r["requests"].as_u64().unwrap()).sum();
    assert_eq!(reqs, 2, "both clients are additive (FR-6.1)");
    let input: u64 = rows
        .iter()
        .map(|r| r["input_tokens"].as_u64().unwrap())
        .sum();
    assert_eq!(input, 600, "sum of distinct input token values");
}

#[test]
fn usage_non_qwen_provider_filter_excludes_both_clients() {
    let sb = Sandbox::new("both-excluded");
    let out = sb.run(&[
        "usage",
        "--since",
        "2026-08-01",
        "--until",
        "2026-08-31",
        "--provider",
        "anthropic",
        "--group-by",
        "provider,model",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "llmu usage --provider anthropic must succeed: {}",
        sb.stderr(&out)
    );
    let rows = sb.json_rows(&out);
    assert!(
        rows.is_empty(),
        "a non-Qwen filter must exclude the Qwen Code row AND the routed Claude Code qwen row (FR-6.1): {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// Source contracts: local Claude events share the same provider filter and
// are filtered before the quota/usage merge (FR-6.1).
// ---------------------------------------------------------------------------

#[test]
fn local_claude_events_share_the_provider_filter() {
    let src = read("src/main.rs");
    assert!(
        src.contains("fn filter_admits("),
        "main.rs must hold one shared pure provider-filter predicate (FR-6.1)"
    );
    assert!(
        src.contains("filter_admits(provider_filter, p.id())"),
        "provider worker selection must use the shared predicate"
    );
    assert!(
        src.contains("local_events.retain(|e| filter_admits(provider_filter"),
        "local Claude events must be retained by the same predicate"
    );
    // Filtering happens before both the rolling quota computation and the
    // usage merge inside the local collector join arm.
    let arm = src
        .split("match h.join()")
        .nth(1)
        .expect("local collector join arm");
    let arm = arm.split("Ok(Err(e))").next().expect("Ok(Ok) arm");
    let retain = arm
        .find("retain(")
        .expect("local events filtered before merge");
    let quota = arm
        .find("rolling_quota")
        .expect("quota computed after the filter");
    let usage = arm
        .find("g.events.extend")
        .expect("usage merge after the filter");
    assert!(retain < quota, "provider filter applies before quota merge");
    assert!(retain < usage, "provider filter applies before usage merge");
}

// ---------------------------------------------------------------------------
// Source contracts: ANSI / TUI / help / type comments enumerate qwen (FR-6).
// ---------------------------------------------------------------------------

#[test]
fn ansi_source_maps_qwen_to_red() {
    let src = read("src/ansi.rs");
    assert!(
        src.contains("\"qwen\" => RED"),
        "ANSI provider_color must map qwen to RED (FR-6)"
    );
}

#[test]
fn tui_source_maps_qwen_to_light_red() {
    let src = read("src/tui.rs");
    assert!(
        src.contains("\"qwen\" => Color::LightRed"),
        "TUI provider_color must map qwen to Color::LightRed (FR-6)"
    );
}

#[test]
fn provider_help_and_type_comments_enumerate_qwen() {
    let main = read("src/main.rs");
    assert!(
        main.contains("(anthropic,openai,deepseek,kimi,glm,gemini,qwen)"),
        "--provider help must enumerate qwen (FR-6)"
    );
    let types = read("src/types.rs");
    assert!(
        types.contains("\"anthropic\" | \"openai\" | \"deepseek\" | \"kimi\" | \"glm\" | \"gemini\" | \"qwen\""),
        "UsageEvent.provider's enumerated comment must include qwen (FR-6)"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/qwen_wiring.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/qwen_wiring.rs is a std-only contract and must not import llmu"
    );
}

// ---------------------------------------------------------------------------
// Hermetic binary sandbox (isolated HOME/XDG, explicit Qwen paths, no
// provider env vars, no network).
// ---------------------------------------------------------------------------

struct Sandbox {
    dir: PathBuf,
    config: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "llmu-qwen-wiring-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for sub in ["home", "config", "data", "qwen-home", "qwen-runtime"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let home = dir.join("home");
        fs::create_dir_all(home.join(".claude/projects/llmu-wiring")).unwrap();

        // Explicit Qwen home/runtime and Claude Code transcripts in config:
        // no settings.json or env fallback participates.
        let qwen_home = dir.join("qwen-home").display().to_string();
        let qwen_runtime = dir.join("qwen-runtime").display().to_string();
        let config = dir.join("config/llmu.toml");
        fs::write(
            &config,
            format!(
                "[claude_code]\nenabled = true\n\n[qwen]\nhome = \"{qwen_home}\"\nruntime_dir = \"{qwen_runtime}\"\n"
            ),
        )
        .unwrap();

        // One Qwen Code request-ledger row.
        let ledger = dir.join("qwen-runtime/usage");
        fs::create_dir_all(&ledger).unwrap();
        fs::write(
            ledger.join("token-usage-2026-08.jsonl"),
            req_record() + "\n",
        )
        .unwrap();

        // One Claude Code transcript assistant row with a qwen* model.
        fs::write(
            home.join(".claude/projects/llmu-wiring/session.jsonl"),
            claude_transcript_row() + "\n",
        )
        .unwrap();

        Sandbox { dir, config }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_llmu"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .env("HOME", self.dir.join("home"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            // Qwen: every credential and path fallback is cleared.
            .env_remove("DASHSCOPE_API_KEY")
            .env_remove("BAILIAN_API_KEY")
            .env_remove("BAILIAN_CODING_PLAN_API_KEY")
            .env_remove("BAILIAN_TOKEN_PLAN_API_KEY")
            .env_remove("QWEN_HOME")
            .env_remove("QWEN_RUNTIME_DIR")
            // Anthropic / Claude / OpenCode.
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("ANTHROPIC_BASE_URL")
            .env_remove("ANTHROPIC_ADMIN_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("OPENCODE_DATA_DIR")
            // OpenAI / Codex.
            .env_remove("OPENAI_ADMIN_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_HOME")
            // DeepSeek.
            .env_remove("DEEPSEEK_API_KEY")
            // Kimi.
            .env_remove("MOONSHOT_API_KEY")
            .env_remove("KIMI_API_KEY")
            .env_remove("KIMI_CODE_API_KEY")
            .env_remove("KIMI_SHARE_DIR")
            // GLM.
            .env_remove("ZAI_API_KEY")
            .env_remove("ZHIPU_API_KEY")
            // Gemini.
            .env_remove("GEMINI_CLI_HOME")
            .env_remove("GOOGLE_CLOUD_PROJECT")
            .env_remove("GOOGLE_CLOUD_PROJECT_ID")
            .output()
            .expect("spawning llmu binary")
    }

    fn stdout(&self, out: &Output) -> String {
        String::from_utf8(out.stdout.clone()).expect("utf8 stdout")
    }

    fn stderr(&self, out: &Output) -> String {
        String::from_utf8(out.stderr.clone()).expect("utf8 stderr")
    }

    fn json_rows(&self, out: &Output) -> Vec<serde_json::Value> {
        let stdout = self.stdout(out);
        let v: serde_json::Value =
            serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("bad JSON ({e}): {stdout}"));
        let rows = v.as_array().expect("usage --json must emit an array");
        assert!(
            rows.iter().all(|r| r["keys"].is_array()),
            "aggregated rows carry their keys: {stdout}"
        );
        rows.clone()
    }
}
