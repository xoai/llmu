//! Task 1: exact bundled rusqlite 0.31.0 pin, one shared OpenCode
//! data-dir resolver, and the post-`apply_qwen` token-plan fallback
//! contracts (RED -> GREEN).
//!
//! Std-only, never importing private `llmu` modules — mirrors
//! `tests/qwen_config.rs` / `tests/http_cache.rs`: source contracts pin
//! the FR-1 / FR-2 / FR-31 / FR-32 surface textually, and black-box tests
//! drive the real binary (`CARGO_BIN_EXE_llmu`) against isolated
//! HOME/XDG directories with every provider env var removed, so no real
//! credential, real-home OpenCode data, or network ever participates
//! (NFR-8). All key values are synthetic and asserted absent from output.

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
// Source contracts (FR-4, NFR-12, binary-only structure, FR-1/2, FR-31/32).
// ---------------------------------------------------------------------------

#[test]
fn cargo_toml_pins_exact_bundled_rusqlite() {
    let cargo = read("Cargo.toml");
    assert!(
        cargo.contains("rusqlite = { version = \"=0.31.0\", features = [\"bundled\"] }"),
        "Cargo.toml must declare `rusqlite = {{ version = \"=0.31.0\", features = [\"bundled\"] }}` (FR-4)"
    );
}

#[test]
fn cargo_lock_pins_rusqlite_031_and_bundled_transitives() {
    let lock = read("Cargo.lock");
    assert!(
        lock.contains("name = \"rusqlite\"") && lock.contains("version = \"0.31.0\""),
        "Cargo.lock must pin rusqlite 0.31.0 (FR-4, NFR-12)"
    );
    assert!(
        lock.contains("name = \"libsqlite3-sys\"") && lock.contains("version = \"0.28.0\""),
        "Cargo.lock must pin the bundled SQLite build dependency libsqlite3-sys 0.28.0 (NFR-12)"
    );
    assert!(
        lock.contains("name = \"cc\""),
        "the bundled SQLite C build needs the `cc` crate in the lockfile (NFR-12)"
    );
    assert!(
        !lock.contains("name = \"bindgen\""),
        "no bindgen/clang runtime requirement may be introduced (NFR-12)"
    );
}

#[test]
fn cargo_toml_has_no_library_target() {
    let cargo = read("Cargo.toml");
    assert!(
        !cargo.contains("[lib]"),
        "llmu stays binary-only: Cargo.toml must not declare a [lib] target"
    );
    assert!(
        fs::metadata(repo_root().join("src/lib.rs")).is_err(),
        "llmu stays binary-only: src/lib.rs must not exist"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/opencode_usage.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/opencode_usage.rs is a std-only contract and must not import llmu"
    );
}

#[test]
fn discover_has_one_shared_opencode_data_dir_resolver() {
    let d = read("src/discover.rs");
    assert!(
        d.contains("pub(crate) fn opencode_data_dir("),
        "src/discover.rs must expose the shared resolver for auth AND the database path (FR-1, FR-2)"
    );
    assert!(
        d.contains("opencode_data_dir(&std_env, dirs::home_dir().as_deref())"),
        "opencode_auth must resolve through the shared resolver (FR-2)"
    );
    assert_eq!(
        d.matches("env(\"OPENCODE_DATA_DIR\")").count(),
        1,
        "the OPENCODE_DATA_DIR precedence exists exactly once (FR-2 — no duplicate implementation)"
    );
    assert_eq!(
        d.matches("env(\"XDG_DATA_HOME\")").count(),
        1,
        "the XDG_DATA_HOME precedence exists exactly once (FR-2 — no duplicate implementation)"
    );
    assert!(
        !d.contains("dirs::home_dir().map(|h| h.join(\".local/share/opencode\"))"),
        "the home fallback lives only inside the resolver (FR-2 — no duplicate implementation)"
    );
}

#[test]
fn discover_opencode_token_plan_fallback_runs_after_apply_qwen() {
    let d = read("src/discover.rs");
    for alias in [
        "alibaba-token-plan",
        "alibaba-token-plan-cn",
        "bailian-token-plan-personal",
    ] {
        assert!(
            d.contains(alias),
            "the OpenCode token-plan fallback must consult `{alias}` in exact priority order (FR-31)"
        );
    }
    assert!(
        d.contains("apply_qwen(cfg"),
        "apply_qwen must still resolve Qwen sources"
    );
    let apply_pos = d.find("apply_qwen(cfg").expect("apply_qwen call");
    let alias_pos = d.find("alibaba-token-plan").expect("alias list");
    assert!(
        apply_pos < alias_pos,
        "the Qwen-specific OpenCode fallback must run after apply_qwen (FR-31)"
    );
    assert!(
        d.contains("cfg.qwen.token_plan_key = Some(v)"),
        "the fallback targets qwen.token_plan_key only — never standard_key or coding_plan_key (FR-31)"
    );
}

#[test]
fn discover_opencode_token_plan_provenance_names_source_without_keys() {
    let d = read("src/discover.rs");
    assert!(
        d.contains("opencode auth "),
        "the fallback provenance must be `qwen.token_plan_key <- opencode auth <source>` with no key value (FR-32)"
    );
    assert!(
        d.contains("qwen.token_plan_key"),
        "the fallback fills the token-plan field only (FR-31)"
    );
}

// ---------------------------------------------------------------------------
// Black-box discovery integration (hermetic, no real-home access).
// ---------------------------------------------------------------------------

/// One hermetic platform-directory sandbox per test. The child process
/// gets an isolated HOME/XDG environment with every provider key removed;
/// the test process environment is never mutated.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "llmu-opencode-usage-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for sub in ["home", "config", "data", "override", "qwen-home"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        Sandbox { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    fn data(&self) -> PathBuf {
        self.dir.join("data")
    }

    fn config(&self) -> PathBuf {
        self.dir.join("config")
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    fn override_dir(&self) -> PathBuf {
        self.dir.join("override")
    }

    fn qwen_home(&self) -> PathBuf {
        self.dir.join("qwen-home")
    }

    fn write_auth(&self, dir: &Path, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join("auth.json");
        fs::write(&p, body).unwrap();
        p
    }

    /// Configured child `Command`: sandbox HOME/XDG_CONFIG_HOME,
    /// `OPENCODE_DATA_DIR` and `XDG_DATA_HOME` per argument, and every
    /// provider env var removed. Tests then add args/env and `.output()`.
    fn cmd(&self, opencode_dir: Option<&Path>, xdg_data_home: Option<&Path>) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_llmu"));
        c.env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.config());
        match opencode_dir {
            Some(d) => {
                c.env("OPENCODE_DATA_DIR", d);
            }
            None => {
                c.env_remove("OPENCODE_DATA_DIR");
            }
        }
        match xdg_data_home {
            Some(d) => {
                c.env("XDG_DATA_HOME", d);
            }
            None => {
                c.env_remove("XDG_DATA_HOME");
            }
        }
        for var in [
            "DASHSCOPE_API_KEY",
            "BAILIAN_API_KEY",
            "BAILIAN_CODING_PLAN_API_KEY",
            "BAILIAN_TOKEN_PLAN_API_KEY",
            "QWEN_HOME",
            "QWEN_RUNTIME_DIR",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "KIMI_SHARE_DIR",
            "DEEPSEEK_API_KEY",
            "MOONSHOT_API_KEY",
            "KIMI_API_KEY",
            "KIMI_CODE_API_KEY",
            "ZAI_API_KEY",
            "ZHIPU_API_KEY",
            "ANTHROPIC_ADMIN_KEY",
            "OPENAI_ADMIN_KEY",
            "GEMINI_CLI_HOME",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
        ] {
            c.env_remove(var);
        }
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(None, Some(&self.data()))
            .args(args)
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

/// NFR-8 guard: no synthetic key, no auth.json path, and no Qwen token
/// plan provenance may appear when no OpenCode data dir exists anywhere
/// in the sandbox — and the developer's real home never leaks into output.
#[test]
fn opencode_auth_missing_everywhere_yields_no_credentials() {
    let sb = Sandbox::new("missing");
    let out = sb.run(&["--config", sb.config_path().to_str().unwrap(), "providers"]);
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    assert!(
        !stdout.contains("auth.json"),
        "no OpenCode auth provenance may appear without a data dir (FR-1):\n{stdout}"
    );
    assert!(
        !stdout.contains("qwen.token_plan_key") && !stdout.contains("opencode auth"),
        "no Qwen token-plan fallback may fire without an OpenCode auth file (FR-31):\n{stdout}"
    );
    assert!(
        !stdout.contains("sk-"),
        "no credential value may leak into output:\n{stdout}"
    );
    if let Ok(real_home) = std::env::var("HOME") {
        if !real_home.is_empty() {
            assert!(
                !stdout.contains(&real_home),
                "the developer's real home must never be consulted or printed (NFR-8):\n{stdout}"
            );
        }
    }
}

/// OPENCODE_DATA_DIR override wins over XDG_DATA_HOME and the home
/// fallback; every existing provider alias plus the three token-plan
/// aliases resolve through the shared data dir, with secret-free
/// provenance (FR-1, FR-2, FR-31, FR-32).
#[test]
fn opencode_override_env_wins_and_all_provider_aliases_map() {
    let sb = Sandbox::new("override");
    let path = sb.write_auth(
        &sb.override_dir(),
        r#"{
            "zai": {"type": "api", "key": "sk-test-zai-open"},
            "deepseek": {"type": "api", "key": "sk-test-deepseek-open"},
            "moonshotai": {"type": "api", "key": "sk-test-moonshot-open"},
            "kimi-for-coding": {"type": "api", "key": "sk-test-kimi-coding-open"},
            "anthropic": {"type": "oauth", "access": "sk-ant-oauth-test-open"},
            "alibaba-token-plan": {"type": "api", "key": "sk-test-alibaba-token-plan"},
            "alibaba-token-plan-cn": {"type": "api", "key": "sk-test-alibaba-token-plan-cn"},
            "bailian-token-plan-personal": {"type": "api", "key": "sk-test-bailian-token-plan-personal"}
        }"#,
    );
    let out = sb
        .cmd(Some(&sb.override_dir()), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap(), "providers"])
        .output()
        .expect("spawning llmu binary");
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let path_str = path.display().to_string();
    assert!(
        stdout.contains(&path_str),
        "the override auth.json path must appear in provenance (FR-1):\n{stdout}"
    );
    for field in [
        "glm.api_key",
        "deepseek.api_key",
        "kimi.api_key",
        "kimi.code_key",
        "claude.access_token",
        "qwen.token_plan_key",
    ] {
        assert!(
            stdout.contains(field),
            "missing {field} in providers output:\n{stdout}"
        );
    }
    assert_eq!(
        stdout.matches(&format!("opencode auth {path_str}")).count(),
        1,
        "the token-plan provenance names the OpenCode auth source exactly once (FR-32):\n{stdout}"
    );
    assert!(
        !stdout.contains("sk-test-") && !sb.stderr(&out).contains("sk-test-"),
        "no key value may leak into diagnostics (NFR Security):\n{stdout}"
    );
}

/// XDG_DATA_HOME joined with `opencode` supplies the auth file when
/// OPENCODE_DATA_DIR is unset (FR-1 fallback 2).
#[test]
fn opencode_xdg_data_home_fallback_finds_auth() {
    let sb = Sandbox::new("xdg");
    let path = sb.write_auth(
        &sb.data().join("opencode"),
        r#"{"alibaba-token-plan": {"type": "api", "key": "sk-test-alibaba-token-plan"}}"#,
    );
    let out = sb.run(&["--config", sb.config_path().to_str().unwrap(), "providers"]);
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let path_str = path.display().to_string();
    assert!(
        stdout.contains(&format!("opencode auth {path_str}")),
        "XDG_DATA_HOME/opencode/auth.json must resolve (FR-1):\n{stdout}"
    );
    assert!(
        !stdout.contains("sk-test-"),
        "no key value may leak into output:\n{stdout}"
    );
}

/// With neither OPENCODE_DATA_DIR nor XDG_DATA_HOME, the user home
/// joined with `.local/share/opencode` supplies the auth file (FR-1
/// fallback 3).
#[test]
fn opencode_default_home_fallback_finds_auth() {
    let sb = Sandbox::new("home");
    let path = sb.write_auth(
        &sb.home().join(".local/share/opencode"),
        r#"{"bailian-token-plan-personal": {"type": "api", "key": "sk-test-bailian-token-plan-personal"}}"#,
    );
    let out = sb
        .cmd(None, None)
        .args(["--config", sb.config_path().to_str().unwrap(), "providers"])
        .output()
        .expect("spawning llmu binary");
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let path_str = path.display().to_string();
    assert!(
        stdout.contains(&format!("opencode auth {path_str}")),
        "~/.local/share/opencode/auth.json must resolve (FR-1):\n{stdout}"
    );
    assert!(
        !stdout.contains("sk-test-"),
        "no key value may leak into output:\n{stdout}"
    );
}

/// An auth.json holding ONLY the three token-plan aliases fills
/// `qwen.token_plan_key` (one provenance line) and touches neither the
/// other provider fields nor standard/coding plan keys (FR-31).
#[test]
fn opencode_token_plan_aliases_fill_only_token_plan_key() {
    let sb = Sandbox::new("aliases-only");
    let path = sb.write_auth(
        &sb.data().join("opencode"),
        r#"{
            "alibaba-token-plan": {"type": "api", "key": "sk-test-alibaba-token-plan"},
            "alibaba-token-plan-cn": {"type": "api", "key": "sk-test-alibaba-token-plan-cn"},
            "bailian-token-plan-personal": {"type": "api", "key": "sk-test-bailian-token-plan-personal"}
        }"#,
    );
    let out = sb.run(&["--config", sb.config_path().to_str().unwrap(), "providers"]);
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let path_str = path.display().to_string();
    assert_eq!(
        stdout.matches(&format!("opencode auth {path_str}")).count(),
        1,
        "the token-plan fallback fires exactly once (FR-31):\n{stdout}"
    );
    for absent in [
        "glm.api_key",
        "deepseek.api_key",
        "kimi.api_key",
        "kimi.code_key",
        "claude.access_token",
        "qwen.standard_key",
        "qwen.coding_plan_key",
    ] {
        assert!(
            !stdout.contains(absent),
            "the token-plan aliases must never map to `{absent}` (FR-31):\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("sk-test-"),
        "no key value may leak into output:\n{stdout}"
    );
}

/// Explicit `[qwen] token_plan_key` config beats the OpenCode fallback
/// (FR-31 precedence: explicit config first).
#[test]
fn opencode_token_plan_fallback_loses_to_explicit_config() {
    let sb = Sandbox::new("cfg-beats");
    sb.write_auth(
        &sb.data().join("opencode"),
        r#"{"alibaba-token-plan": {"type": "api", "key": "sk-test-alibaba-token-plan"}}"#,
    );
    fs::write(
        sb.config_path(),
        "[qwen]\ntoken_plan_key = \"sk-cfg-token-plan\"\n",
    )
    .unwrap();
    let out = sb.run(&["--config", sb.config_path().to_str().unwrap(), "providers"]);
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    assert!(
        !stdout.contains("qwen.token_plan_key") && !stdout.contains("opencode auth"),
        "explicit config token_plan_key must precede the OpenCode fallback (FR-31):\n{stdout}"
    );
    assert!(
        !stdout.contains("sk-cfg-token-plan") && !stdout.contains("sk-test-"),
        "no key value may leak into output:\n{stdout}"
    );
}

/// Environment `BAILIAN_TOKEN_PLAN_API_KEY` beats the OpenCode fallback
/// and reports its own provenance (FR-31 precedence: env after config).
#[test]
fn opencode_token_plan_fallback_loses_to_env() {
    let sb = Sandbox::new("env-beats");
    sb.write_auth(
        &sb.data().join("opencode"),
        r#"{"alibaba-token-plan": {"type": "api", "key": "sk-test-alibaba-token-plan"}}"#,
    );
    let out = sb
        .cmd(None, Some(&sb.data()))
        .env("BAILIAN_TOKEN_PLAN_API_KEY", "sk-env-token-plan")
        .args(["--config", sb.config_path().to_str().unwrap(), "providers"])
        .output()
        .expect("spawning llmu binary");
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    assert!(
        stdout.contains("env BAILIAN_TOKEN_PLAN_API_KEY"),
        "the env source must fill and report token_plan_key (FR-1):\n{stdout}"
    );
    assert!(
        !stdout.contains("opencode auth"),
        "env BAILIAN_TOKEN_PLAN_API_KEY must precede the OpenCode fallback (FR-31):\n{stdout}"
    );
    assert!(
        !stdout.contains("sk-env-token-plan") && !stdout.contains("sk-test-"),
        "no key value may leak into output:\n{stdout}"
    );
}

/// Qwen Code settings `env.BAILIAN_TOKEN_PLAN_API_KEY` beats the OpenCode
/// fallback and reports its own provenance (FR-31 precedence: Qwen Code
/// settings after env).
#[test]
fn opencode_token_plan_fallback_loses_to_qwen_settings() {
    let sb = Sandbox::new("settings-beats");
    sb.write_auth(
        &sb.data().join("opencode"),
        r#"{"alibaba-token-plan": {"type": "api", "key": "sk-test-alibaba-token-plan"}}"#,
    );
    fs::write(
        sb.config_path(),
        format!("[qwen]\nhome = \"{}\"\n", sb.qwen_home().display()),
    )
    .unwrap();
    fs::write(
        sb.qwen_home().join("settings.json"),
        r#"{"env":{"BAILIAN_TOKEN_PLAN_API_KEY":"sk-settings-token-plan"}}"#,
    )
    .unwrap();
    let out = sb.run(&["--config", sb.config_path().to_str().unwrap(), "providers"]);
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let settings_path = sb.qwen_home().join("settings.json").display().to_string();
    assert!(
        stdout.contains("qwen.token_plan_key") && stdout.contains(&settings_path),
        "Qwen Code settings must fill and report token_plan_key (FR-1):\n{stdout}"
    );
    assert!(
        !stdout.contains("opencode auth"),
        "Qwen Code settings must precede the OpenCode fallback (FR-31):\n{stdout}"
    );
    assert!(
        !stdout.contains("sk-settings-token-plan") && !stdout.contains("sk-test-"),
        "no key value may leak into output:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// Task 2 source contracts (FR-3, FR-5, FR-6, FR-8, FR-9, FR-10, FR-11, FR-12,
// FR-25, FR-34, NFR-1, NFR-3, NFR-5, NFR-6/7, NFR-9). The collector is not
// wired into the binary yet, so these pin the foundation textually; the
// synthetic temporary-database behavior tests live inline in
// `src/local/opencode.rs` where the private seam is reachable (NFR-8).
// ---------------------------------------------------------------------------

/// Production portion of `src/local/opencode.rs` (before `#[cfg(test)]`):
/// inline fixtures legitimately carry synthetic write SQL (NFR-8), so the
/// collector's query-shape contracts scan production only.
fn opencode_prod() -> String {
    let o = read("src/local/opencode.rs");
    match o.find("#[cfg(test)]") {
        Some(i) => o[..i].to_string(),
        None => o,
    }
}

#[test]
fn local_owns_shared_collected_and_exports_opencode() {
    let m = read("src/local/mod.rs");
    assert!(
        m.contains("pub mod opencode;"),
        "local/mod.rs must export the opencode collector"
    );
    assert!(
        m.contains("pub struct Collected"),
        "local/mod.rs owns the shared Collected (FR-25)"
    );
    assert!(
        m.contains("pub events") && m.contains("pub notes"),
        "Collected carries events + notes (FR-25)"
    );
    let cc = read("src/local/claude_code.rs");
    assert!(
        !cc.contains("pub struct Collected"),
        "claude_code must use the shared Collected, not define its own (FR-25)"
    );
    assert!(
        cc.contains("use super::Collected") || cc.contains("use crate::local::Collected"),
        "claude_code refers to the shared Collected (FR-25)"
    );
}

#[test]
fn opencode_db_path_uses_shared_resolver_without_duplication() {
    let o = opencode_prod();
    assert!(
        o.contains("opencode_data_dir"),
        "the database path resolves through the shared resolver (FR-3)"
    );
    assert!(
        o.contains("opencode.db"),
        "the database file is opencode.db (FR-3)"
    );
    assert!(
        !o.contains("OPENCODE_DATA_DIR"),
        "no duplicate OPENCODE_DATA_DIR precedence logic (FR-2)"
    );
    assert!(
        !o.contains("XDG_DATA_HOME"),
        "no duplicate XDG_DATA_HOME precedence logic (FR-2)"
    );
}

#[test]
fn opencode_open_flags_are_exactly_readonly_nomutex() {
    let o = opencode_prod();
    assert!(
        o.contains("SQLITE_OPEN_READ_ONLY"),
        "the database opens READONLY (FR-5)"
    );
    assert!(
        o.contains("SQLITE_OPEN_NO_MUTEX"),
        "the database opens NOMUTEX (FR-5)"
    );
    for absent in [
        "SQLITE_OPEN_READ_WRITE",
        "SQLITE_OPEN_CREATE",
        "SQLITE_OPEN_URI",
        "immutable",
    ] {
        assert!(
            !o.contains(absent),
            "forbidden open flag `{absent}` (FR-5/FR-7)"
        );
    }
}

#[test]
fn opencode_busy_timeout_bounded_250ms() {
    let o = opencode_prod();
    assert!(
        o.contains("busy_timeout(Duration::from_millis(250))"),
        "the busy timeout is exactly 250 ms (FR-6, NFR-3)"
    );
}

#[test]
fn opencode_query_only_enabled_and_verified() {
    let o = opencode_prod();
    assert!(
        o.contains("pragma_update(None, \"query_only\", true)"),
        "query_only is set ON (FR-6)"
    );
    assert!(
        o.contains("pragma_query_value(None, \"query_only\""),
        "query_only is verified after being set (FR-6)"
    );
    assert_eq!(
        o.matches("pragma_update").count(),
        1,
        "exactly one pragma set — nothing else may mutate connection state (FR-6)"
    );
}

#[test]
fn opencode_query_is_single_streamed_message_read_with_exact_bounds() {
    let o = opencode_prod();
    assert!(
        o.contains("SELECT time_created, data FROM message"),
        "selects only time_created and data from message (FR-9)"
    );
    assert!(
        o.contains("time_created >= ?1"),
        "the lower bound is inclusive `>=` (FR-10)"
    );
    assert!(
        o.contains("time_created < ?2"),
        "the upper bound is exclusive `<` (FR-10)"
    );
    assert!(
        o.contains("ORDER BY time_created, id"),
        "deterministic order by time then id (FR-9/FR-10)"
    );
    assert_eq!(
        o.matches("FROM ").count(),
        1,
        "exactly one table read — message only (FR-9, NFR-1)"
    );
    for absent in [
        "FROM event",
        "FROM part",
        "FROM transcript",
        "SELECT *",
        "JOIN",
        "GROUP BY",
        "LIMIT",
    ] {
        assert!(
            !o.contains(absent),
            "forbidden query shape `{absent}` (FR-9, NFR-1)"
        );
    }
}

#[test]
fn opencode_has_no_write_sql_or_ddl() {
    let o = opencode_prod();
    for absent in [
        "INSERT",
        "UPDATE message",
        "DELETE FROM",
        "CREATE TABLE",
        "DROP TABLE",
        "ALTER TABLE",
        "journal_mode",
        "VACUUM",
        "ATTACH",
        "REPLACE INTO",
    ] {
        assert!(
            !o.contains(absent),
            "no write SQL or DDL may exist (FR-6/NFR-1/NFR-9): `{absent}`"
        );
    }
}

#[test]
fn opencode_metadata_notfound_is_silent_absence_and_other_failures_categorized() {
    let o = opencode_prod();
    assert!(
        o.contains("std::fs::metadata"),
        "metadata is probed before open (FR-3)"
    );
    assert!(
        o.contains("ErrorKind::NotFound"),
        "NotFound is the one normal-absence path (FR-3)"
    );
    for cat in ["Missing", "Unreadable", "Busy", "Incompatible"] {
        assert!(
            o.contains(&format!("DbError::{cat}")),
            "failure category DbError::{cat} must exist (FR-8)"
        );
    }
}

#[test]
fn opencode_notes_are_fixed_and_secret_free() {
    let o = opencode_prod();
    assert!(
        o.contains("opencode:"),
        "notes are bounded `opencode:` diagnostics (FR-8)"
    );
    assert_eq!(
        o.matches("notes.push(format!(").count(),
        2,
        "only malformed and unsupported aggregate counts may format notes (FR-33)"
    );
    assert!(
        !o.contains("format!(\"opencode: local usage database"),
        "database notes stay fixed and never interpolate raw errors (FR-35)"
    );
}

#[test]
fn opencode_exposes_streaming_row_seam() {
    let o = opencode_prod();
    assert!(
        o.contains("pub fn stream_message_rows"),
        "the row seam is public for Task 3 (FR-11/FR-12)"
    );
    assert!(
        o.contains(".query(rusqlite::params!"),
        "rows flow through the rusqlite row iterator (FR-11)"
    );
    assert!(
        !o.contains("collect::<Vec<"),
        "production never materializes rows (FR-11)"
    );
}

#[test]
fn opencode_collect_returns_shared_collected() {
    let o = opencode_prod();
    assert!(
        o.contains("pub fn collect"),
        "a collect entry exists for later wiring (FR-25)"
    );
    assert!(
        o.contains("Collected {"),
        "collect builds the shared Collected (FR-25)"
    );
}

#[test]
fn opencode_task3_declares_strict_parser_allowlist_aggregation_and_final_notes() {
    let o = opencode_prod();
    for needle in [
        "fn parse_record",
        "fn canonical_provider",
        "saturating_add",
        "estimate_cost",
        "SourceKind::LocalLogs",
        "opencode: skipped {malformed} malformed local usage record(s)",
        "opencode: skipped {unsupported} local usage record(s) from unsupported provider(s)",
        "opencode: local usage database is busy or unreadable",
        "opencode: local usage database schema is unsupported",
    ] {
        assert!(
            o.contains(needle),
            "Task 3 production must contain `{needle}`"
        );
    }
}

#[test]
fn opencode_task3_allowlist_contains_every_reviewed_alias_and_no_model_inference() {
    let o = opencode_prod();
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
            o.contains(&format!("\"{alias}\"")),
            "missing provider alias {alias}"
        );
    }
    assert!(
        !o.contains("starts_with(\"qwen"),
        "provider attribution never uses model prefixes"
    );
}

#[test]
fn opencode_task3_ignores_client_cost_and_preserves_fresh_input() {
    let o = opencode_prod();
    assert!(
        !o.contains("[\"cost\"]"),
        "OpenCode client cost is never read"
    );
    assert!(
        !o.contains("saturating_sub"),
        "cache is never subtracted from fresh input"
    );
}
