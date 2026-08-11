//! Task 6: offline daily balance history contracts (RED -> GREEN).
//!
//! Std-only, never importing private `llmu` modules. Mirrors the
//! `tests/http_cache.rs` / `tests/quota_provenance.rs` pattern: source
//! contracts pin the FR-2 surface textually, and black-box tests drive
//! the real binary (`CARGO_BIN_EXE_llmu`) against isolated
//! XDG data/config/home directories so no real credential or network
//! ever participates.

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
// Source contracts (FR-2): the shape the binary must expose.
// ---------------------------------------------------------------------------

#[test]
fn types_declare_balance_history_row() {
    let t = read("src/types.rs");
    for needle in [
        "pub struct BalanceHistoryRow",
        "pub from: chrono::NaiveDate",
        "pub to: chrono::NaiveDate",
        "pub provider: String",
        "pub currency: String",
        "pub opening: f64",
        "pub closing: f64",
        "pub spent: f64",
        "pub funded: f64",
    ] {
        assert!(
            t.contains(needle),
            "src/types.rs must declare `{needle}` (FR-2.4 BalanceHistoryRow)"
        );
    }
}

#[test]
fn main_declares_offline_history_dispatch() {
    let m = read("src/main.rs");
    for needle in [
        "history: bool",
        "handle_balance",
        "store::read_balance_history",
        "failed to append balance snapshot",
    ] {
        assert!(
            m.contains(needle),
            "src/main.rs must `{needle}` (FR-2.1 / FR-2.9 balance --history dispatch)"
        );
    }
    let discard = ["record_balances(&g.balances)", ".ok()"].concat();
    assert!(
        !m.contains(&discard),
        "FR-2.9: snapshot-write failures must be surfaced, never silently discarded"
    );
}

#[test]
fn report_renders_balance_history_table() {
    let r = read("src/report.rs");
    assert!(
        r.contains("pub fn render_balance_history"),
        "src/report.rs must expose `render_balance_history` (FR-2.7 human rows)"
    );
}

#[test]
fn csv_belongs_to_task_8_and_is_not_introduced_here() {
    for f in [
        "src/main.rs",
        "src/report.rs",
        "src/types.rs",
        "src/store.rs",
    ] {
        assert!(
            !read(f).contains("csv"),
            "{f} must not introduce CSV output (Task 8 owns CSV, FR-1)"
        );
    }
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/balance_history.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/balance_history.rs is a std-only contract and must not import llmu"
    );
}

// ---------------------------------------------------------------------------
// Black-box CLI contracts: isolated XDG data/config/home directories.
// ---------------------------------------------------------------------------

/// One hermetic platform-directory sandbox per test: `XDG_DATA_HOME` is
/// the platform data dir the binary resolves via `dirs::data_dir()`, so
/// `balances.jsonl` lives at `<sandbox>/data/llmu/balances.jsonl`.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "llmu-balance-history-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("data/llmu")).unwrap();
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::create_dir_all(dir.join("home")).unwrap();
        Sandbox { dir }
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.join("data")
    }

    fn balances_path(&self) -> PathBuf {
        self.dir.join("data/llmu/balances.jsonl")
    }

    fn write_history(&self, lines: &[&str]) {
        fs::write(self.balances_path(), lines.join("\n") + "\n").unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_llmu"))
            .args(args)
            .env("XDG_DATA_HOME", self.data_dir())
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("HOME", self.dir.join("home"))
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

/// Fixture exercising AS-5 in one file: out-of-order lines, same-day
/// duplicates, a same-timestamp tie (later physical line wins), gaps,
/// funding increase, equality, provider/currency isolation, and two
/// malformed lines (one not JSON, one non-finite total).
fn fixture_lines() -> Vec<&'static str> {
    vec![
        r#"{"ts":"2026-08-03T20:00:00Z","provider":"deepseek","currency":"USD","total":8.0,"granted":0.0,"topped_up":0.0}"#,
        r#"{"ts":"2026-08-01T12:00:00Z","provider":"deepseek","currency":"USD","total":10.0,"granted":0.0,"topped_up":0.0}"#,
        r#"{"ts":"2026-08-01T18:00:00Z","provider":"deepseek","currency":"USD","total":9.5,"granted":0.0,"topped_up":0.0}"#,
        r#"{"ts":"2026-08-02T00:00:00Z","provider":"deepseek","currency":"USD","total":9.0,"granted":0.0,"topped_up":0.0}"#,
        r#"{"ts":"2026-08-01T23:30:00Z","provider":"kimi","currency":"USD","total":5.0,"granted":0.0,"topped_up":0.0}"#,
        r#"{"ts":"2026-08-02T23:30:00Z","provider":"kimi","currency":"USD","total":7.5,"granted":0.0,"topped_up":0.0}"#,
        r#"{"ts":"2026-08-02T23:30:00Z","provider":"kimi","currency":"USD","total":7.0,"granted":0.0,"topped_up":0.0}"#,
        r#"{"ts":"2026-08-01T08:00:00Z","provider":"deepseek","currency":"CNY","total":100.0,"granted":0.0,"topped_up":0.0}"#,
        r#"{"ts":"2026-08-02T08:00:00Z","provider":"deepseek","currency":"CNY","total":100.0,"granted":0.0,"topped_up":0.0}"#,
        "not json at all",
        r#"{"ts":"2026-08-04T00:00:00Z","provider":"deepseek","currency":"USD","total":1e999}"#,
    ]
}

#[test]
fn missing_history_file_is_an_empty_history() {
    let sb = Sandbox::new("missing");
    let out = sb.run(&["balance", "--history"]);
    assert!(out.status.success());
    assert_eq!(
        sb.stdout(&out),
        "no balance history (llmu/balances.jsonl missing or empty)\n"
    );
    assert!(
        sb.stderr(&out).is_empty(),
        "a missing file must not produce a malformed-record note"
    );
    let _ = fs::remove_dir_all(&sb.dir);
}

#[test]
fn history_json_derives_daily_intervals_deterministically() {
    let sb = Sandbox::new("json");
    sb.write_history(&fixture_lines());
    let out = sb.run(&["balance", "--history", "--json"]);
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        sb.stderr(&out)
    );
    let v: serde_json::Value =
        serde_json::from_str(&sb.stdout(&out)).expect("stdout must be exactly one JSON array");
    let rows = v.as_array().expect("history JSON is an array");
    assert_eq!(rows.len(), 4);
    // Sorted by (to, provider, currency, from) — FR-2.6.
    assert_eq!(rows[0]["to"], "2026-08-02");
    assert_eq!(rows[0]["from"], "2026-08-01");
    assert_eq!(rows[0]["provider"], "deepseek");
    assert_eq!(rows[0]["currency"], "CNY");
    assert_eq!(rows[0]["opening"], 100.0);
    assert_eq!(rows[0]["closing"], 100.0);
    assert_eq!(rows[0]["spent"], 0.0);
    assert_eq!(rows[0]["funded"], 0.0);
    assert_eq!(rows[1]["provider"], "deepseek");
    assert_eq!(rows[1]["currency"], "USD");
    assert_eq!(rows[1]["opening"], 9.5);
    assert_eq!(rows[1]["closing"], 9.0);
    assert_eq!(rows[1]["spent"], 0.5);
    assert_eq!(rows[1]["funded"], 0.0);
    assert_eq!(rows[2]["provider"], "kimi");
    assert_eq!(rows[2]["currency"], "USD");
    assert_eq!(rows[2]["opening"], 5.0);
    assert_eq!(rows[2]["closing"], 7.0);
    assert_eq!(rows[2]["spent"], 0.0);
    assert_eq!(rows[2]["funded"], 2.0);
    assert_eq!(rows[3]["to"], "2026-08-03");
    assert_eq!(rows[3]["from"], "2026-08-02");
    assert_eq!(rows[3]["provider"], "deepseek");
    assert_eq!(rows[3]["opening"], 9.0);
    assert_eq!(rows[3]["closing"], 8.0);
    assert_eq!(rows[3]["spent"], 1.0);
    assert_eq!(rows[3]["funded"], 0.0);
    // Malformed records are summarized once on stderr, never in the JSON.
    assert!(
        sb.stderr(&out).contains("skipped 2 malformed"),
        "stderr note must count malformed records, got: {:?}",
        sb.stderr(&out)
    );
    let _ = fs::remove_dir_all(&sb.dir);
}

#[test]
fn history_table_consumes_the_same_normalized_rows() {
    let sb = Sandbox::new("table");
    sb.write_history(&fixture_lines());
    let out = sb.run(&["balance", "--history"]);
    assert!(out.status.success());
    let stdout = sb.stdout(&out);
    for needle in [
        "from", "to", "provider", "currency", "opening", "closing", "spent", "funded",
    ] {
        assert!(
            stdout.contains(needle),
            "table must have a `{needle}` column"
        );
    }
    for needle in [
        "deepseek",
        "kimi",
        "CNY",
        "2026-08-01",
        "2026-08-03",
        "9.50",
        "7.00",
        "0.50",
        "2.00",
    ] {
        assert!(stdout.contains(needle), "table must show `{needle}`");
    }
    let _ = fs::remove_dir_all(&sb.dir);
}

#[test]
fn history_makes_no_network_and_never_appends() {
    let sb = Sandbox::new("offline");
    sb.write_history(&fixture_lines());
    let before = fs::read(sb.balances_path()).unwrap();
    let out = sb.run(&["balance", "--history", "--json"]);
    assert!(out.status.success());
    assert_eq!(
        fs::read(sb.balances_path()).unwrap(),
        before,
        "balance --history must never append a snapshot (FR-2.1)"
    );
    let stdout = sb.stdout(&out);
    assert!(
        stdout.trim_start().starts_with('['),
        "stdout must be pure JSON history"
    );
    let stderr = sb.stderr(&out);
    for noise in ["deepseek: balance", "kimi: balance", "worker panicked"] {
        assert!(
            !stderr.contains(noise),
            "no provider balance fetch may run on the history path, got note `{noise}`"
        );
    }
    let _ = fs::remove_dir_all(&sb.dir);
}
