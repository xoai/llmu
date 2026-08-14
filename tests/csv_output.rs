//! Task 8: failing CSV output contracts (RED).
//!
//! Std-only, never importing private `llmu` modules — the established
//! `tests/balance_history.rs` / `tests/cache_wiring.rs` pattern. Source
//! contracts pin the FR-1 clap/report surface textually; black-box tests
//! drive the real binary (`CARGO_BIN_EXE_llmu`) against isolated
//! XDG data/config/home directories so no real credential, network, or
//! balance-store mutation ever participates. The contracts fail because
//! `--csv` does not exist on any report command yet.

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
// Source contracts (FR-1): the shape the binary must expose.
// ---------------------------------------------------------------------------

#[test]
fn main_declares_csv_flags_conflicting_with_json() {
    let m = read("src/main.rs");
    let lines: Vec<&str> = m.lines().collect();
    let decls = lines
        .windows(2)
        .filter(|w| w[0].contains("conflicts_with = \"json\"") && w[1].trim() == "csv: bool,")
        .count();
    assert_eq!(
        decls, 3,
        "usage, balance, and quota must each declare `--csv` conflicting with `--json` (FR-1.1/1.2)"
    );
}

#[test]
fn report_exposes_shared_csv_renderers() {
    let r = read("src/report.rs");
    for needle in [
        "pub fn csv_field",
        "pub fn csv_row",
        "pub fn render_csv",
        "pub fn render_usage_csv",
        "pub fn render_balance_csv",
        "pub fn render_quota_csv",
        "pub fn render_balance_history_csv",
    ] {
        assert!(
            r.contains(needle),
            "src/report.rs must expose `{needle}` (FR-1: shared RFC 4180 rendering + command serializers)"
        );
    }
}

#[test]
fn csv_rendering_is_std_only_with_no_new_dependency() {
    let cargo = read("Cargo.toml");
    assert!(
        !cargo.contains("csv ="),
        "CSV must be implemented by hand (RFC 4180, std-only) — no csv crate dependency (FR-1.3)"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/csv_output.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/csv_output.rs is a std-only contract and must not import llmu"
    );
}

// ---------------------------------------------------------------------------
// Black-box CLI contracts: isolated XDG data/config/home directories.
// ---------------------------------------------------------------------------

/// One hermetic platform-directory sandbox per test: `XDG_DATA_HOME` is
/// the platform data dir the binary resolves via `dirs::data_dir()`, so
/// `balances.jsonl` lives at `<sandbox>/data/llmu/balances.jsonl`;
/// `HOME` is where the local Claude Code transcript walker looks
/// (`~/.claude/projects`); `OPENCODE_DATA_DIR` is pinned to an empty
/// sandbox subdir so the OpenCode collector resolves an isolated (missing)
/// database — no real data dir, no inherited override, no network. No
/// credential env var survives the run (NFR-8).
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "llmu-csv-output-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("data/llmu")).unwrap();
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::create_dir_all(dir.join("opencode")).unwrap();
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.join("home")
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

    /// Claude Code JSONL transcripts under the isolated HOME, so the
    /// offline local collector produces usage events without any network.
    fn write_logs(&self, lines: &[String]) {
        let dir = self.home().join(".claude/projects/demo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_llmu"))
            .args(args)
            .env("XDG_DATA_HOME", self.data_dir())
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("HOME", self.home())
            .env("OPENCODE_DATA_DIR", self.dir.join("opencode"))
            .env_remove("DEEPSEEK_API_KEY")
            .env_remove("MOONSHOT_API_KEY")
            .env_remove("KIMI_API_KEY")
            .env_remove("KIMI_CODE_API_KEY")
            .env_remove("KIMI_SHARE_DIR")
            .env_remove("ZAI_API_KEY")
            .env_remove("ZHIPU_API_KEY")
            .env_remove("DASHSCOPE_API_KEY")
            .env_remove("BAILIAN_API_KEY")
            .env_remove("BAILIAN_CODING_PLAN_API_KEY")
            .env_remove("BAILIAN_TOKEN_PLAN_API_KEY")
            .env_remove("QWEN_HOME")
            .env_remove("QWEN_RUNTIME_DIR")
            .env_remove("ANTHROPIC_ADMIN_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("ANTHROPIC_BASE_URL")
            .env_remove("OPENAI_ADMIN_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GOOGLE_CLOUD_PROJECT")
            .env_remove("GOOGLE_CLOUD_PROJECT_ID")
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

/// Four transcript lines: two known-priced Claude models plus two
/// RFC-4180 escape exercises (a model containing LF, and one containing
/// comma + quotes). `base` is the newest instant; the events sit at
/// `base - 1h` down to `base - 4h`. The usage test passes a fixed base
/// that straddles midnight UTC, so the exact-match output also proves
/// `aggregate` sorts the prior period first (FR-1.4); the quota test
/// passes `Utc::now()` so the 5h local quota window sees every event.
fn fixture_lines(base: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    let t = |h: i64| (base - chrono::Duration::hours(h)).to_rfc3339();
    let mk = |ts: String, model: String, input: u64, output: u64, cr: u64, cw: u64| {
        serde_json::json!({
            "timestamp": ts,
            "requestId": format!("req-{model}"),
            "message": {
                "id": format!("msg-{model}"),
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_input_tokens": cr,
                    "cache_creation_input_tokens": cw,
                },
                "content": []
            }
        })
        .to_string()
    };
    vec![
        mk(t(1), "claude-sonnet-4-5".into(), 1_000_000, 0, 0, 0),
        mk(t(2), "claude-haiku-4-5".into(), 200_000, 0, 0, 0),
        mk(t(3), "lf\nmodel".into(), 1, 1, 1, 1),
        mk(t(4), "my, \"weird\" model".into(), 2, 3, 4, 5),
    ]
}

/// Fixed instants straddling midnight UTC (2026-08-01T22:00/23:00Z and
/// 2026-08-02T00:00/01:00Z), so the deterministic prior-period-first
/// row order is exactly the case that used to flake around 01:00-03:59
/// UTC with now-relative timestamps.
fn fixed_fixture_base() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-08-02T02:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

/// Task 5 (NFR-8): the CSV gather harness pins `OPENCODE_DATA_DIR` to an
/// empty sandbox subdir, so the OpenCode collector resolves a missing
/// database — silent, no events, no notes — and the existing Claude CSV
/// regressions stay byte-for-byte exact.
#[test]
fn opencode_sandbox_isolation_keeps_claude_csv_regressions_exact() {
    let sb = Sandbox::new("opencode-isolation");
    sb.write_logs(&fixture_lines(fixed_fixture_base()));
    let out = sb.run(&[
        "usage",
        "--csv",
        "--group-by",
        "source,model",
        "--since",
        "2026-07-01",
    ]);
    assert!(out.status.success());
    let stderr = sb.stderr(&out);
    assert!(
        !stderr.contains("opencode:"),
        "a missing sandbox OpenCode DB must stay silent in CSV mode: {stderr}"
    );
    assert!(
        !stderr.contains("note:"),
        "healthy usage CSV must not leak any notes, got: {stderr:?}"
    );
    let _ = fs::remove_dir_all(&sb.dir);
}

#[test]
fn usage_csv_emits_exact_machine_header_and_rows() {
    let sb = Sandbox::new("usage");
    sb.write_logs(&fixture_lines(fixed_fixture_base()));
    let out = sb.run(&[
        "usage",
        "--csv",
        "--group-by",
        "source,model",
        "--since",
        "2026-07-01",
    ]);
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        sb.stderr(&out)
    );
    let expected = "period,source,model,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_tokens,tool_calls,est_cost_usd,has_cost\n\
         2026-08-01,local,\"lf\nmodel\",1,1,1,1,1,4,0,0,false\n\
         2026-08-01,local,\"my, \"\"weird\"\" model\",1,2,3,4,5,14,0,0,false\n\
         2026-08-02,local,claude-haiku-4-5,1,200000,0,0,0,200000,0,0.2,true\n\
         2026-08-02,local,claude-sonnet-4-5,1,1000000,0,0,0,1000000,0,3,true\n";
    assert_eq!(
        sb.stdout(&out),
        expected,
        "exact ordered output: the 08-01 period rows must sort before the 08-02 rows (FR-1.4)"
    );
    assert!(
        !sb.stdout(&out).contains("Billed"),
        "usage CSV must never add billed-cost rows (FR-1.4)"
    );
    let stderr = sb.stderr(&out);
    assert!(
        !stderr.contains("note:"),
        "healthy usage CSV must not leak notes onto stderr, got: {stderr:?}"
    );
    let _ = fs::remove_dir_all(&sb.dir);
}

#[test]
fn usage_csv_empty_results_still_emit_header_with_notes_on_stderr() {
    let sb = Sandbox::new("usage-empty");
    let out = sb.run(&["usage", "--csv"]);
    assert!(out.status.success());
    assert_eq!(
        sb.stdout(&out),
        "period,provider,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_tokens,tool_calls,est_cost_usd,has_cost\n",
        "empty usage must still emit exactly the header (FR-1.7)"
    );
    let stderr = sb.stderr(&out);
    assert!(
        stderr.contains("note:") && stderr.contains("claude-code"),
        "the missing-transcripts diagnostic is a stderr note, got: {stderr:?}"
    );
    assert!(
        !stderr.contains("period,"),
        "notes must never contaminate CSV content (FR-1.8)"
    );
    let _ = fs::remove_dir_all(&sb.dir);
}

#[test]
fn balance_csv_empty_results_still_emit_header_and_never_mutate_the_store() {
    let sb = Sandbox::new("balance-empty");
    let out = sb.run(&["balance", "--csv"]);
    assert!(out.status.success());
    assert_eq!(
        sb.stdout(&out),
        "provider,total,granted,topped_up,currency\n",
        "empty balance must still emit exactly the header (FR-1.7)"
    );
    assert!(
        !sb.stdout(&out).contains("no balance sources"),
        "the human empty-state message must not leak into CSV"
    );
    assert!(
        !sb.balances_path().exists(),
        "empty results must not mutate the balance store"
    );
    let _ = fs::remove_dir_all(&sb.dir);
}

#[test]
fn quota_csv_emits_local_rows_and_keeps_headers_on_empty() {
    let sb = Sandbox::new("quota-rows");
    sb.write_logs(&fixture_lines(chrono::Utc::now()));
    let out = sb.run(&["quota", "--csv"]);
    assert!(out.status.success());
    assert_eq!(
        sb.stdout(&out),
        "provider,plan,window,used,limit,unit,resets_at\n\
         anthropic,\"Claude Code (local, 4 msgs)\",5h,1200018,0,tokens,\n",
        "quota CSV rows come from the same snapshots; RFC 3339 or empty resets_at (FR-1.6)"
    );
    let _ = fs::remove_dir_all(&sb.dir);

    let sb2 = Sandbox::new("quota-empty");
    let out2 = sb2.run(&["quota", "--csv"]);
    assert!(out2.status.success());
    assert_eq!(
        sb2.stdout(&out2),
        "provider,plan,window,used,limit,unit,resets_at\n",
        "empty quota must still emit exactly the header (FR-1.7)"
    );
    assert!(
        sb2.stderr(&out2).contains("note:"),
        "quota diagnostics stay on stderr while stdout stays pure CSV (FR-1.8)"
    );
    let _ = fs::remove_dir_all(&sb2.dir);
}

#[test]
fn history_csv_emits_normalized_rows() {
    let sb = Sandbox::new("history");
    sb.write_history(&[
        r#"{"ts":"2026-08-10T12:00:00Z","provider":"deepseek","currency":"USD","total":10.0,"granted":0.0,"topped_up":0.0}"#,
        r#"{"ts":"2026-08-11T12:00:00Z","provider":"deepseek","currency":"USD","total":7.5,"granted":0.0,"topped_up":0.0}"#,
    ]);
    let out = sb.run(&["balance", "--history", "--csv"]);
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status.code(),
        sb.stderr(&out)
    );
    assert_eq!(
        sb.stdout(&out),
        "from,to,provider,currency,opening,closing,spent,funded\n\
         2026-08-10,2026-08-11,deepseek,USD,10,7.5,2.5,0\n",
        "history CSV consumes the same normalized rows as table/JSON (FR-2.7)"
    );
    let _ = fs::remove_dir_all(&sb.dir);
}

#[test]
fn history_csv_empty_results_still_emit_header() {
    let sb = Sandbox::new("history-empty");
    let out = sb.run(&["balance", "--history", "--csv"]);
    assert!(out.status.success());
    assert_eq!(
        sb.stdout(&out),
        "from,to,provider,currency,opening,closing,spent,funded\n",
        "empty history must still emit exactly the header (FR-1.7)"
    );
    let _ = fs::remove_dir_all(&sb.dir);
}

#[test]
fn json_csv_conflict_is_rejected_before_any_work() {
    let sb = Sandbox::new("conflict-usage");
    let out = sb.run(&["usage", "--json", "--csv"]);
    assert!(
        !out.status.success(),
        "--json --csv must be rejected (FR-1.2)"
    );
    assert!(
        sb.stdout(&out).is_empty(),
        "a clap rejection must print nothing to stdout"
    );
    assert!(
        sb.stderr(&out).contains("cannot be used with"),
        "clap must name the conflict, got: {:?}",
        sb.stderr(&out)
    );
    let _ = fs::remove_dir_all(&sb.dir);

    let sb2 = Sandbox::new("conflict-history");
    sb2.write_history(&[r#"{"ts":"2026-08-10T12:00:00Z","provider":"deepseek","currency":"USD","total":10.0,"granted":0.0,"topped_up":0.0}"#]);
    let before = fs::read(sb2.balances_path()).unwrap();
    let out2 = sb2.run(&["balance", "--history", "--json", "--csv"]);
    assert!(
        !out2.status.success(),
        "--json --csv on balance --history must be rejected (FR-1.2)"
    );
    assert_eq!(
        fs::read(sb2.balances_path()).unwrap(),
        before,
        "the rejection must precede any balance-store mutation"
    );
    assert!(
        sb2.stderr(&out2).contains("cannot be used with"),
        "clap must name the conflict, got: {:?}",
        sb2.stderr(&out2)
    );
    let _ = fs::remove_dir_all(&sb2.dir);
}
