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
use std::time::{Duration, Instant};

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
        cargo.contains("rusqlite = { version = \"=0.31.0\", features = [\"bundled\", \"functions\"] }"),
        "Cargo.toml must declare `rusqlite = {{ version = \"=0.31.0\", features = [\"bundled\", \"functions\"] }}` (FR-4)"
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
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "OPENAI_API_KEY",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_PROJECT_ID",
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

/// The collector's own query region (Task 4): the Task 2 query-shape
/// contracts live there alone, because the availability probe
/// legitimately adds a second bounded `message` read with `LIMIT 1`.
fn collector_region(o: &str) -> String {
    let start = o
        .find("const MESSAGE_QUERY")
        .expect("collector query const");
    let end = o.find("const AVAILABLE_QUERY").unwrap_or(o.len());
    o[start..end].to_string()
}

#[test]
fn opencode_query_is_single_streamed_message_read_with_exact_bounds() {
    let o = opencode_prod();
    let region = collector_region(&o);
    assert!(
        o.contains("SELECT time_created, data FROM message"),
        "selects only time_created and data from message (FR-9)"
    );
    assert!(
        region.contains("time_created >= ?1"),
        "the lower bound is inclusive `>=` (FR-10)"
    );
    assert!(
        region.contains("time_created < ?2"),
        "the upper bound is exclusive `<` (FR-10)"
    );
    assert!(
        region.contains("ORDER BY time_created, id"),
        "deterministic order by time then id (FR-9/FR-10)"
    );
    assert_eq!(
        region.matches("FROM ").count(),
        1,
        "exactly one table read in the collector query — message only (FR-9, NFR-1)"
    );
    assert_eq!(
        region.matches("SELECT").count(),
        1,
        "exactly one collection SELECT — one streaming bounded-window read (NFR-1)"
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
            !region.contains(absent),
            "forbidden query shape `{absent}` in the collector query (FR-9, NFR-1)"
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

// ---------------------------------------------------------------------------
// Task 4: bounded availability probe contracts (FR-13 parity, no collector
// invocation, constant-select LIMIT 1, `opencode yes/no` row).
// ---------------------------------------------------------------------------

/// The availability probe region of production `src/local/opencode.rs`:
/// the probe's SQL constant, the testable path-injected probe, and the
/// production std probe — ending where the failure classifier begins.
/// The bounded constant-select and no-collector contracts are pinned
/// here — never on the whole file, which legitimately contains the
/// collector query.
fn probe_region(o: &str) -> String {
    let start = o
        .find("const AVAILABLE_QUERY")
        .expect("availability probe query const");
    let end = o.find("fn classify(").expect("failure classifier fn");
    o[start..end].to_string()
}

#[test]
fn opencode_availability_exposes_testable_and_production_probes() {
    let o = opencode_prod();
    assert!(
        o.contains("pub fn available_from(path: &Path) -> bool"),
        "a testable path-injected probe must exist (Task 4)"
    );
    assert!(
        o.contains("pub fn available() -> bool"),
        "a production std probe must exist (Task 4)"
    );
    let probe = probe_region(&o);
    assert!(
        probe.contains("metadata_class"),
        "the probe reuses the metadata safety check (Task 4)"
    );
    assert!(
        probe.contains("open_readonly"),
        "the probe reuses the shared READONLY|NOMUTEX + 250 ms + query_only connection (Task 4)"
    );
    assert!(
        o.contains("std_db_path"),
        "the production probe resolves through the shared std path (Task 4)"
    );
}

#[test]
fn opencode_availability_probe_is_constant_select_with_limit_1() {
    let probe = probe_region(&opencode_prod());
    assert_eq!(
        probe.matches("SELECT").count(),
        1,
        "the probe runs exactly one SELECT (Task 4)"
    );
    assert!(
        probe.contains("SELECT 1") && probe.contains("LIMIT 1"),
        "the probe is `SELECT 1 ... LIMIT 1` returning a constant (Task 4)"
    );
    assert_eq!(
        probe.matches("FROM message").count(),
        1,
        "the probe reads the message table once and nothing else (Task 4)"
    );
    for absent in [
        "SELECT *",
        "JOIN",
        "GROUP BY",
        "ORDER BY",
        "time_created >= ",
        "time_created < ",
        "collect::<Vec<",
    ] {
        assert!(
            !probe.contains(absent),
            "the probe must not contain `{absent}` (Task 4)"
        );
    }
}

#[test]
fn opencode_availability_probe_never_calls_the_collector() {
    let probe = probe_region(&opencode_prod());
    for forbidden in [
        "collect_from",
        "collect(",
        "stream_message_rows",
        "parse_record",
        "canonical_provider",
        "MESSAGE_QUERY",
    ] {
        assert!(
            !probe.contains(forbidden),
            "the availability probe must not invoke `{forbidden}` (Task 4)"
        );
    }
    assert!(
        !probe.contains("fn collect") && !probe.contains("fn stream_message_rows"),
        "no collector code may live inside the probe (Task 4)"
    );
    assert!(
        probe.contains("query_row"),
        "the probe reads at most one row through rusqlite (Task 4)"
    );
}

#[test]
fn opencode_availability_probe_is_scalar_filtered_limit_1_without_lexical_scraping() {
    let probe = probe_region(&opencode_prod());
    assert!(
        probe.contains(
            "SELECT 1 FROM message WHERE strict_eligible(time_created, data) = 1 LIMIT 1"
        ),
        "the probe is one scalar-filtered constant-1 LIMIT 1 read (FR-30)"
    );
    assert!(
        probe.contains("create_scalar_function"),
        "the probe registers a connection-local scalar (FR-30)"
    );
    let compact: String = probe.split_whitespace().collect();
    assert!(
        compact.contains("FunctionFlags::SQLITE_UTF8|FunctionFlags::SQLITE_DETERMINISTIC|FunctionFlags::SQLITE_DIRECTONLY"),
        "the scalar is UTF8, deterministic, and direct-only (FR-30)"
    );
    assert!(
        probe.contains("get_raw"),
        "the scalar reads raw typed arguments (FR-30)"
    );
    for absent in [
        "json_extract",
        "json_type",
        "TRIM(",
        "instr(",
        "substr(",
        "LIKE ",
        "typeof(",
        "18446744073709551615",
        "18446744073709551616",
    ] {
        assert!(
            !probe.contains(absent),
            "no lexical JSON scraping may remain in the probe: `{absent}` (FR-30)"
        );
    }
}

// ---------------------------------------------------------------------------
// Task 4 black-box: `llmu providers` status row over isolated synthetic
// databases (NFR-8). Integration crates may use package dependencies, so
// rusqlite writes the synthetic `message` table directly; llmu stays
// binary-only and is never imported.
// ---------------------------------------------------------------------------

fn write_opencode_db(dir: &Path, rows: &[(i64, &str, &str)]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("opencode.db");
    let w = rusqlite::Connection::open(&path).unwrap();
    w.execute_batch(
        "CREATE TABLE message (id TEXT PRIMARY KEY, sessionID TEXT, time_created INTEGER, \
         time_updated INTEGER, role TEXT, providerID TEXT, modelID TEXT, data TEXT)",
    )
    .unwrap();
    for (t, id, data) in rows {
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, t, data],
        )
        .unwrap();
    }
    drop(w);
    path
}

fn eligible_message(provider: &str) -> String {
    format!(
        r#"{{"role":"assistant","providerID":"{provider}","modelID":"model-a","time":{{"created":1000,"completed":1001}},"finish":"stop","tokens":{{"input":10,"output":20,"reasoning":5,"cache":{{"read":3,"write":2}}}}}}"#
    )
}

/// An eligible OpenCode SQLite record prints `opencode yes` with the
/// honest capability wording; no path, key, or diagnostic leaks.
#[test]
fn providers_prints_opencode_yes_for_eligible_db() {
    let sb = Sandbox::new("avail-yes");
    let dir = sb.dir.join("eligible-data");
    write_opencode_db(&dir, &[(1000, "m1", &eligible_message("openai"))]);
    let out = sb
        .cmd(Some(&dir), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap(), "providers"])
        .output()
        .expect("spawning llmu binary");
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let row = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("opencode"))
        .expect("providers must list an opencode row");
    assert!(
        row.contains("yes"),
        "an eligible OpenCode DB must report configured yes:\n{row}"
    );
    assert!(
        row.contains(
            "read-only local usage from OpenCode's SQLite message records across supported providers"
        ),
        "the capabilities wording must be exact:\n{row}"
    );
    assert!(
        !stdout.contains("opencode.db") && !stdout.contains(dir.display().to_string().as_str()),
        "no path diagnostics may reach output (Task 4):\n{stdout}"
    );
    assert!(
        !sb.stderr(&out).contains("opencode"),
        "no opencode diagnostics on stderr (Task 4): {}",
        sb.stderr(&out)
    );
}

/// Rejected-only and missing databases print `opencode no` with no
/// diagnostics of any kind.
#[test]
fn providers_prints_opencode_no_for_rejected_and_missing_db() {
    let sb = Sandbox::new("avail-no");
    let rejected_dir = sb.dir.join("rejected-data");
    let user_row = r#"{"role":"user","providerID":"openai","modelID":"model-a","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#;
    write_opencode_db(&rejected_dir, &[(1000, "m1", user_row)]);
    let out = sb
        .cmd(Some(&rejected_dir), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap(), "providers"])
        .output()
        .expect("spawning llmu binary");
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let row = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("opencode"))
        .expect("providers must list an opencode row");
    assert!(row.contains("no"), "a rejected-only DB answers no:\n{row}");
    assert!(
        !sb.stderr(&out).contains("note:"),
        "status never surfaces diagnostics: {}",
        sb.stderr(&out)
    );

    let missing_dir = sb.dir.join("missing-data");
    let out = sb
        .cmd(Some(&missing_dir), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap(), "providers"])
        .output()
        .expect("spawning llmu binary");
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let row = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("opencode"))
        .expect("providers must list an opencode row");
    assert!(row.contains("no"), "a missing DB answers no:\n{row}");
    assert!(
        !sb.stderr(&out).contains("note:"),
        "status never surfaces diagnostics: {}",
        sb.stderr(&out)
    );
}

/// A corrupt database still prints `opencode no`; raw sqlite error text
/// and paths never leak into stdout or stderr.
#[test]
fn providers_opencode_status_never_leaks_db_diagnostics() {
    let sb = Sandbox::new("avail-corrupt");
    let dir = sb.dir.join("corrupt-data");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("opencode.db"), "this is not a sqlite database").unwrap();
    let out = sb
        .cmd(Some(&dir), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap(), "providers"])
        .output()
        .expect("spawning llmu binary");
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let row = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("opencode"))
        .expect("providers must list an opencode row");
    assert!(row.contains("no"), "a corrupt DB answers no:\n{row}");
    let combined = format!("{stdout}\n{}", sb.stderr(&out));
    for leak in [
        "not a sqlite",
        "malformed",
        "no such table",
        "disk image",
        "opencode.db",
    ] {
        assert!(
            !combined.contains(leak),
            "status must never surface DB diagnostics: `{leak}` in:\n{combined}"
        );
    }
}

// ---------------------------------------------------------------------------
// Task 5: usage delivery — black-box OpenCode CLI coverage (FR-21, FR-23,
// FR-24, FR-26-28, §17 CLI tests). The synthetic SQLite helpers above are
// reused; fixed UTC windows only, no now-based membership except the bare
// overview whose window is inherently now-relative (no paths/IDs/dates in
// expected diagnostics).
// ---------------------------------------------------------------------------

const WINDOW_SINCE: &str = "2026-08-01";
const WINDOW_UNTIL: &str = "2026-08-31";

fn ms_at(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap()
        .with_timezone(&chrono::Utc)
        .timestamp_millis()
}

/// One strict-valid OpenCode assistant record whose `time.created` equals
/// the database `time_created` (FR-13). `tokens` is
/// `[input, output, reasoning, cache_read, cache_write]` — all in fixed
/// buckets (input, output+reasoning, cache read, cache write).
fn record(provider: &str, model: &str, created_ms: i64, tokens: [u64; 5]) -> String {
    format!(
        r#"{{"role":"assistant","providerID":"{provider}","modelID":"{model}","time":{{"created":{created_ms},"completed":{completed}}},"finish":"stop","tokens":{{"input":{input},"output":{output},"reasoning":{reasoning},"cache":{{"read":{cr},"write":{cw}}}}}}}"#,
        completed = created_ms + 1,
        input = tokens[0],
        output = tokens[1],
        reasoning = tokens[2],
        cr = tokens[3],
        cw = tokens[4]
    )
}

fn run_usage(sb: &Sandbox, db: &Path, extra: &[&str]) -> Output {
    // `OPENCODE_DATA_DIR` is the data DIRECTORY; the collector joins
    // `opencode.db` (FR-3), so the file path's parent is the env value.
    let mut cmd = sb.cmd(Some(db.parent().unwrap()), Some(&sb.data()));
    cmd.args([
        "--config",
        sb.config_path().to_str().unwrap(),
        "usage",
        "--since",
        WINDOW_SINCE,
        "--until",
        WINDOW_UNTIL,
    ]);
    cmd.args(extra);
    cmd.output().expect("spawning llmu binary")
}

fn usage_rows(sb: &Sandbox, db: &Path, extra: &[&str]) -> Vec<serde_json::Value> {
    let out = run_usage(sb, db, extra);
    assert!(
        out.status.success(),
        "llmu usage must succeed: {}",
        sb.stderr(&out)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("bad JSON ({e}): {}", sb.stdout(&out)));
    let rows = v.as_array().expect("usage --json emits an array");
    assert!(
        rows.iter().all(|r| r["keys"].is_array()),
        "aggregated rows carry their keys: {}",
        sb.stdout(&out)
    );
    rows.clone()
}

/// One synthetic DB with all 17 reviewed aliases, each under a distinct
/// model so aggregation cannot merge them and coverage stays observable.
fn alias_db(dir: &Path) -> PathBuf {
    let t = ms_at("2026-08-10T10:00:00Z");
    let aliases: [&str; 17] = [
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
    ];
    let owned: Vec<(i64, String, String)> = aliases
        .iter()
        .enumerate()
        .map(|(i, alias)| {
            let at = t + i as i64;
            (
                at,
                format!("id-{alias}"),
                record(alias, &format!("m-{alias}"), at, [10, 20, 5, 3, 2]),
            )
        })
        .collect();
    let refs: Vec<(i64, &str, &str)> = owned
        .iter()
        .map(|(t, id, d)| (*t, id.as_str(), d.as_str()))
        .collect();
    write_opencode_db(dir, &refs)
}

fn models(rows: &[serde_json::Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["keys"][2].as_str().unwrap().to_string())
        .collect()
}

/// FR-21 + FR-19: every canonical `--provider` filter admits all of its
/// aliases and excludes every other alias; unknown filters admit nothing.
#[test]
fn opencode_alias_filters_admit_every_canonical_provider_and_exclude_others() {
    let sb = Sandbox::new("t5-aliases");
    let db = alias_db(&sb.dir.join("aliases"));

    let rows = usage_rows(&sb, &db, &["--group-by", "provider,model", "--json"]);
    assert_eq!(rows.len(), 17, "all 17 reviewed aliases emit events");
    let providers: Vec<String> = rows
        .iter()
        .map(|r| r["keys"][1].as_str().unwrap().to_string())
        .collect();
    for p in ["qwen", "glm", "deepseek", "kimi", "openai"] {
        assert!(
            providers.contains(&p.to_string()),
            "canonical provider {p} must appear in the unfiltered report"
        );
    }

    for (provider, expected) in [
        (
            "qwen",
            &[
                "m-alibaba",
                "m-alibaba-cn",
                "m-alibaba-coding-plan",
                "m-alibaba-coding-plan-cn",
                "m-alibaba-token-plan",
                "m-alibaba-token-plan-cn",
                "m-bailian-token-plan-personal",
            ][..],
        ),
        (
            "glm",
            &[
                "m-zai",
                "m-zai-coding-plan",
                "m-zhipuai",
                "m-zhipuai-coding-plan",
            ][..],
        ),
        ("deepseek", &["m-deepseek"][..]),
        (
            "kimi",
            &["m-kimi-for-coding", "m-moonshot", "m-moonshotai", "m-kimi"][..],
        ),
        ("openai", &["m-openai"][..]),
    ] {
        let rows = usage_rows(
            &sb,
            &db,
            &[
                "--provider",
                provider,
                "--group-by",
                "provider,model",
                "--json",
            ],
        );
        let got = models(&rows);
        assert_eq!(got.len(), expected.len(), "{provider} filter: {got:?}");
        for m in expected {
            assert!(
                got.contains(&m.to_string()),
                "{provider} must admit {m}, got {got:?}"
            );
        }
    }

    let rows = usage_rows(
        &sb,
        &db,
        &[
            "--provider",
            "anthropic",
            "--group-by",
            "provider,model",
            "--json",
        ],
    );
    assert!(
        rows.is_empty(),
        "a non-OpenCode provider filter must exclude every alias: {rows:?}"
    );
}

/// `--model` is a case-insensitive substring; `--source local` admits
/// OpenCode events, `--source api` excludes them; the report shows the
/// `local` provenance through the source group key.
#[test]
fn opencode_model_filter_case_insensitive_and_source_filters() {
    let sb = Sandbox::new("t5-filters");
    let t = ms_at("2026-08-10T10:00:00Z");
    let db = write_opencode_db(
        &sb.dir.join("filters"),
        &[(
            t,
            "id-1",
            record("alibaba", "Qwen-Max", t, [10, 20, 5, 3, 2]).as_str(),
        )],
    );

    let rows = usage_rows(
        &sb,
        &db,
        &[
            "--model",
            "qwen-m",
            "--group-by",
            "provider,model",
            "--json",
        ],
    );
    assert_eq!(
        rows.len(),
        1,
        "lowercase substring must match case-insensitively"
    );
    assert_eq!(rows[0]["keys"][2], "Qwen-Max");

    let rows = usage_rows(
        &sb,
        &db,
        &[
            "--model",
            "QWEN-MAX",
            "--group-by",
            "provider,model",
            "--json",
        ],
    );
    assert_eq!(
        rows.len(),
        1,
        "uppercase substring must match case-insensitively"
    );

    let rows = usage_rows(
        &sb,
        &db,
        &["--model", "zzz", "--group-by", "provider,model", "--json"],
    );
    assert!(
        rows.is_empty(),
        "a non-matching model substring admits nothing"
    );

    let rows = usage_rows(
        &sb,
        &db,
        &[
            "--source",
            "local",
            "--group-by",
            "provider,source",
            "--json",
        ],
    );
    assert_eq!(rows.len(), 1, "--source local admits OpenCode events");
    assert_eq!(
        rows[0]["keys"][2], "local",
        "the source group key shows local provenance"
    );

    let rows = usage_rows(
        &sb,
        &db,
        &["--source", "api", "--group-by", "provider,source", "--json"],
    );
    assert!(
        rows.is_empty(),
        "--source api must exclude OpenCode local events"
    );
}

/// FR-23: OpenCode events are additive with local Claude transcripts —
/// no cross-source dedup. The two fixtures share provider/model/hour, so
/// aggregation merges them and the merged row carries both sources' sums.
#[test]
fn opencode_adds_to_local_claude_transcripts_without_dedup() {
    let sb = Sandbox::new("t5-additive");
    let t = ms_at("2026-08-11T12:00:00Z");
    let db = write_opencode_db(
        &sb.dir.join("add"),
        &[(
            t,
            "id-open",
            record("alibaba", "qwen-max", t, [1500, 800, 200, 1000, 0]).as_str(),
        )],
    );
    let dir = sb.home().join(".claude/projects/demo");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("session.jsonl"),
        r#"{"timestamp":"2026-08-11T12:00:00Z","requestId":"cc-req-1","message":{"id":"cc-msg-1","model":"qwen-max","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":25,"cache_creation_input_tokens":0},"content":[]}}"#
            .to_string()
            + "\n",
    )
    .unwrap();

    let rows = usage_rows(
        &sb,
        &db,
        &[
            "--provider",
            "qwen",
            "--group-by",
            "provider,model",
            "--json",
        ],
    );
    assert_eq!(
        rows.len(),
        1,
        "both sources share provider/model/hour and merge into one additive row: {rows:?}"
    );
    let r = &rows[0];
    assert_eq!(r["requests"], 2, "requests add across sources, no dedup");
    assert_eq!(
        r["input_tokens"], 1600,
        "input adds (1500 opencode + 100 transcript)"
    );
    assert_eq!(
        r["output_tokens"], 1050,
        "output adds (1000 opencode + 50 transcript)"
    );
    assert_eq!(r["cache_read_tokens"], 1025, "cache read adds (1000 + 25)");
    assert_eq!(r["total_tokens"], 3675);
}

/// FR-33/35 + §17: malformed and unsupported records yield exactly the
/// bounded stderr notes; stdout stays pure JSON/CSV with the valid rows;
/// no path/id/provider/model/SQL/JSON fragment ever reaches stderr.
#[test]
fn opencode_malformed_and_unsupported_notes_bounded_and_never_leak() {
    let sb = Sandbox::new("t5-notes");
    let t = ms_at("2026-08-10T10:00:00Z");
    let db = write_opencode_db(
        &sb.dir.join("notes"),
        &[
            (
                t,
                "id-ok",
                record("openai", "model-a", t, [10, 20, 5, 3, 2]).as_str(),
            ),
            (
                t + 1,
                "id-bad",
                r#"{"role":"assistant","providerID":"openai""#,
            ),
            (
                t + 2,
                "id-unknown",
                record("google", "model-g", t + 2, [1, 1, 0, 0, 0]).as_str(),
            ),
        ],
    );

    let out = run_usage(&sb, &db, &["--group-by", "provider,model", "--json"]);
    assert!(out.status.success(), "{}", sb.stderr(&out));
    let stdout = sb.stdout(&out);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("pure JSON stdout");
    assert_eq!(rows.len(), 1, "only the valid record reaches the report");
    assert_eq!(rows[0]["keys"][1], "openai");
    assert_eq!(rows[0]["keys"][2], "model-a");
    assert!(
        !stdout.contains("opencode: skipped"),
        "notes must never contaminate JSON stdout"
    );
    let stderr = sb.stderr(&out);
    assert!(
        stderr.contains("note: opencode: skipped 1 malformed local usage record(s)"),
        "the malformed note must be exactly bounded: {stderr}"
    );
    assert!(
        stderr.contains(
            "note: opencode: skipped 1 local usage record(s) from unsupported provider(s)"
        ),
        "the unsupported-provider note must be exactly bounded: {stderr}"
    );
    assert_eq!(
        stderr.matches("note: opencode:").count(),
        2,
        "exactly two opencode notes, one per category: {stderr}"
    );
    for leak in [
        "google",
        "model-g",
        "model-a",
        "providerID",
        "modelID",
        "id-unknown",
        "INSERT",
        "CREATE",
        "opencode.db",
        "session.jsonl",
    ] {
        assert!(
            !stderr.contains(leak),
            "stderr must not leak `{leak}`: {stderr}"
        );
    }
    assert!(
        !stderr.contains(&sb.dir.display().to_string()),
        "stderr must not leak sandbox paths"
    );

    let out = run_usage(&sb, &db, &["--group-by", "provider,model", "--csv"]);
    assert!(out.status.success(), "{}", sb.stderr(&out));
    assert_eq!(
        sb.stdout(&out),
        "period,provider,model,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_tokens,tool_calls,est_cost_usd,has_cost\n\
         2026-08-10,openai,model-a,1,10,25,3,2,40,0,0,false\n",
        "CSV stays pure and exact while notes go to stderr"
    );
    assert!(
        !sb.stdout(&out).contains("opencode: skipped"),
        "notes must never contaminate CSV stdout"
    );
}

/// §17: JSON/CSV schemas stay byte-exact with OpenCode rows present.
#[test]
fn opencode_usage_json_and_csv_schemas_are_unchanged() {
    let sb = Sandbox::new("t5-schema");
    let t = ms_at("2026-08-10T10:00:00Z");
    let db = write_opencode_db(
        &sb.dir.join("schema"),
        &[(
            t,
            "id-1",
            record("openai", "model-a", t, [10, 20, 5, 3, 2]).as_str(),
        )],
    );
    let rows = usage_rows(&sb, &db, &["--group-by", "provider,model", "--json"]);
    let mut keys: Vec<&str> = rows[0]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "cache_read_tokens",
            "cache_write_tokens",
            "est_cost_usd",
            "has_cost",
            "input_tokens",
            "keys",
            "output_tokens",
            "requests",
            "tool_calls",
            "total_tokens",
        ],
        "the JSON schema is the exact existing shape"
    );

    let out = run_usage(&sb, &db, &["--group-by", "provider,model", "--csv"]);
    assert!(out.status.success(), "{}", sb.stderr(&out));
    assert_eq!(
        sb.stdout(&out),
        "period,provider,model,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_tokens,tool_calls,est_cost_usd,has_cost\n\
         2026-08-10,openai,model-a,1,10,25,3,2,40,0,0,false\n"
    );
}

/// FR-26/NFR-8: the collector must run only when `want_usage` — quota and
/// balance gathers never touch the OpenCode database, so a
/// diagnostic-producing DB emits no `opencode:` note on those commands.
#[test]
fn opencode_collector_stays_gated_for_quota_and_balance() {
    let sb = Sandbox::new("t5-gated");
    let t = ms_at("2026-08-10T10:00:00Z");
    let db = write_opencode_db(
        &sb.dir.join("gated"),
        &[(t, "id-bad", r#"{"role":"assistant","providerID":"openai""#)],
    );

    let out = sb
        .cmd(Some(db.parent().unwrap()), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap(), "quota"])
        .output()
        .expect("spawning llmu binary");
    assert!(out.status.success(), "quota: {}", sb.stderr(&out));
    assert!(
        !sb.stderr(&out).contains("opencode:"),
        "quota (want_usage=false) must not run the OpenCode collector: {}",
        sb.stderr(&out)
    );

    let out = sb
        .cmd(Some(db.parent().unwrap()), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap(), "balance"])
        .output()
        .expect("spawning llmu binary");
    assert!(out.status.success(), "balance: {}", sb.stderr(&out));
    assert!(
        !sb.stderr(&out).contains("opencode:"),
        "balance (want_usage=false) must not run the OpenCode collector: {}",
        sb.stderr(&out)
    );

    let out = run_usage(&sb, &db, &["--json"]);
    assert!(
        sb.stderr(&out)
            .contains("note: opencode: skipped 1 malformed local usage record(s)"),
        "sanity: the same DB must produce the bounded note when usage runs: {}",
        sb.stderr(&out)
    );
}

/// FR-24 + §17: maximum-token OpenCode fixtures saturate every bucket and
/// the derived total to `u64::MAX` through JSON and CSV with unchanged
/// headers, and render the human table grand total without panic.
#[test]
fn opencode_max_tokens_saturate_json_csv_and_table() {
    let sb = Sandbox::new("t5-saturation");
    let t = ms_at("2026-08-10T10:00:00Z");
    let max_row = record("alibaba", "m-max", t, [u64::MAX, u64::MAX, 10, u64::MAX, 5]);
    let pos_row = record("alibaba", "m-pos", t + 60_000, [1, 1, 1, 1, 1]);
    let db = write_opencode_db(
        &sb.dir.join("sat"),
        &[
            (t, "id-max", max_row.as_str()),
            (t + 60_000, "id-pos", pos_row.as_str()),
        ],
    );

    let rows = usage_rows(&sb, &db, &["--group-by", "provider,model", "--json"]);
    assert_eq!(rows.len(), 2);
    let max_r = rows.iter().find(|r| r["keys"][2] == "m-max").unwrap();
    assert_eq!(max_r["requests"], 1);
    assert_eq!(max_r["tool_calls"], 0, "OpenCode rows carry no tool calls");
    assert_eq!(max_r["input_tokens"], serde_json::json!(u64::MAX));
    assert_eq!(
        max_r["output_tokens"],
        serde_json::json!(u64::MAX),
        "output+reasoning saturates"
    );
    assert_eq!(max_r["cache_read_tokens"], serde_json::json!(u64::MAX));
    assert_eq!(max_r["cache_write_tokens"], 5);
    assert_eq!(
        max_r["total_tokens"],
        serde_json::json!(u64::MAX),
        "the derived total saturates"
    );
    let pos_r = rows.iter().find(|r| r["keys"][2] == "m-pos").unwrap();
    assert_eq!(pos_r["input_tokens"], 1);
    assert_eq!(pos_r["output_tokens"], 2, "1 output + 1 reasoning");
    assert_eq!(pos_r["total_tokens"], 5);

    let out = run_usage(&sb, &db, &["--group-by", "provider,model", "--csv"]);
    assert!(out.status.success(), "{}", sb.stderr(&out));
    assert_eq!(
        sb.stdout(&out),
        "period,provider,model,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_tokens,tool_calls,est_cost_usd,has_cost\n\
         2026-08-10,qwen,m-max,1,18446744073709551615,18446744073709551615,18446744073709551615,5,18446744073709551615,0,0,false\n\
         2026-08-10,qwen,m-pos,1,1,2,1,1,5,0,0,false\n",
        "CSV keeps the exact header and machine values, saturated"
    );

    let out = run_usage(&sb, &db, &["--group-by", "provider,model"]);
    assert!(
        out.status.success(),
        "the human table must not panic on saturated values: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    assert!(
        stdout.contains("18,446,744,073,709,551,615"),
        "the table grand total must saturate without panic:\n{stdout}"
    );
    assert!(
        stdout.contains("TOTAL"),
        "the grand total row renders:\n{stdout}"
    );
}

/// Bare `llmu` overview: OpenCode events render with `local` provenance
/// and saturated values never panic the table (FR-24, §17).
#[test]
fn opencode_overview_shows_local_provenance_and_saturates_without_panic() {
    let sb = Sandbox::new("t5-overview");
    let t = chrono::Utc::now().timestamp_millis() - 2 * 60 * 60 * 1000;
    let db = write_opencode_db(
        &sb.dir.join("ov"),
        &[(
            t,
            "id-ov",
            record(
                "alibaba",
                "qwen-max",
                t,
                [u64::MAX, u64::MAX, 10, u64::MAX, 5],
            )
            .as_str(),
        )],
    );
    let out = sb
        .cmd(Some(db.parent().unwrap()), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap()])
        .output()
        .expect("spawning llmu binary");
    assert!(
        out.status.success(),
        "overview must not panic: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    assert!(
        stdout.contains("usage counted from: qwen (local logs)"),
        "the overview scope line must show OpenCode local provenance:\n{stdout}"
    );
    assert!(
        stdout.contains("18,446,744,073,709,551,615"),
        "saturated totals render without panic/wrap:\n{stdout}"
    );
}

/// FR-26/FR-23 (additive-API note): llmu's test suite has no local HTTP
/// stub, so OpenCode+API additivity is pinned structurally — OpenCode
/// events join the identical shared usage stream as provider API events
/// (the same `g.events.extend` merge) — and the existing
/// provider-additivity regression (`tests/qwen_wiring.rs`) stays green.
#[test]
fn gather_merges_opencode_events_into_the_shared_usage_stream() {
    let src = read("src/main.rs");
    assert!(
        src.contains("local::opencode::collect"),
        "gather must spawn the OpenCode collector (FR-26)"
    );
    assert!(
        src.contains("fn merge_opencode"),
        "a fail-soft OpenCode merge must exist (FR-26)"
    );
    let arm = src.split("fn merge_opencode").nth(1).expect("merge body");
    let merge = arm.split("#[cfg(test)]").next().expect("production merge");
    assert!(
        merge.contains("g.events.extend"),
        "OpenCode events join the shared usage stream like every provider (FR-23/26)"
    );
    assert!(
        merge.contains("g.notes.extend"),
        "OpenCode notes append unfiltered (FR-26)"
    );
    assert!(
        merge.contains("filter_admits"),
        "OpenCode events pass the shared provider filter (FR-21)"
    );
    assert!(
        merge.contains("parser panicked"),
        "a panicked OpenCode worker yields a bounded fixed note (FR-26)"
    );
}

/// FR-27/FR-28/AC-5: the TUI local tick owns OpenCode partial-refresh
/// collection. It calls the collector directly (no gather, no provider
/// filter, no availability precursor), recognizes exactly the two fixed
/// database-level notes to downgrade completion so prior local rows
/// survive, and never logs the (secret-free) notes.
#[test]
fn opencode_tui_local_tick_collects_directly_and_preserves_snapshot_on_db_failure() {
    let src = read("src/tui.rs");
    let local_tick = src
        .split("} else if !is_paused && last_local.elapsed() >= locald {")
        .nth(1)
        .and_then(|section| section.split("std::thread::sleep").next())
        .expect("local refresh branch");

    assert!(
        local_tick.contains("local::opencode::collect"),
        "the local tick must call the OpenCode collector directly (FR-27)"
    );
    assert!(
        !local_tick.contains("crate::gather"),
        "the local tick must not invoke the general provider gather path"
    );
    assert!(
        !local_tick.contains("Some(&filt)"),
        "a provider filter is CLI report semantics, not local-source selection"
    );
    assert!(
        !local_tick.contains("available"),
        "the local tick must not run an availability precursor probe"
    );
    assert!(
        !local_tick.contains("println") && !local_tick.contains("eprintln"),
        "collection notes are never logged by the TUI"
    );

    for note in [
        "opencode: local usage database is busy or unreadable",
        "opencode: local usage database schema is unsupported",
    ] {
        assert!(
            src.contains(note),
            "the TUI must pin the fixed DB-failure note: {note}"
        );
    }
    let predicate = src
        .split("fn opencode_db_failed")
        .nth(1)
        .and_then(|arm| arm.split("\n}\n").next())
        .expect("opencode_db_failed body");
    assert!(
        predicate.contains("opencode: local usage database is busy or unreadable")
            && predicate.contains("opencode: local usage database schema is unsupported"),
        "the predicate must match exactly the two fixed DB-failure notes"
    );
    assert!(
        !predicate.contains("skipped"),
        "bounded skip notes must never downgrade the refresh"
    );
    assert!(
        !predicate.contains("apply_local_refresh"),
        "the predicate only classifies notes; it must not apply state"
    );
}

// ---------------------------------------------------------------------------
// Task 7: WAL / busy / privacy / source hardening contracts. The WAL-only
// committed-row behavior fixtures live inline in `src/local/opencode.rs`
// (private seams); these contracts pin source shape, cross-file parity,
// secret-free diagnostics, and the busy status black-box.
// ---------------------------------------------------------------------------

/// The stale `#[allow(dead_code)] // ... lands in Task 5` annotations were
/// Task 2/3 scaffolding: `collect` is wired into `gather` (FR-26) and
/// `available` into `providers` (FR-29), so no dead-code allow or
/// not-yet-reachable comment may remain in the collector (plan Task 7).
#[test]
fn opencode_collector_has_no_stale_dead_code_allows() {
    let o = opencode_prod();
    assert!(
        !o.contains("#[allow(dead_code)]"),
        "the collector is reachable via gather and providers; no stale dead-code allow may remain"
    );
    for stale in [
        "not yet reachable",
        "wired by gather/CLI in Task 5",
        "lands in Task 5",
    ] {
        assert!(
            !o.contains(stale),
            "stale wiring-comment marker `{stale}` must be gone"
        );
    }
}

/// NFR-9 + §17: production `src/local/opencode.rs` must stay free of
/// scraping markers (browser/console/cookie/sec_token), snapshot/copy
/// behavior, and SQL write/DDL/migration/vacuum/attach/detach vocabulary.
/// The scan targets precise code markers, never bare words, so the
/// read-only doc comments ("never copies or snapshots") cannot false
/// positive; the `query_only` verification itself is separately pinned
/// above and must not be trapped here.
#[test]
fn opencode_production_has_no_scraping_snapshot_or_migration_markers() {
    let o = opencode_prod();
    for absent in [
        "browser",
        "console",
        "cookie",
        "sec_token",
        "fs::copy",
        "snapshot(",
        "migration",
        "DETACH",
        "detach",
    ] {
        assert!(
            !o.contains(absent),
            "forbidden production marker `{absent}` (NFR-9/§17)"
        );
    }
}

/// FR-35 + §17: adversarial records carrying sentinel secret/path/id/
/// provider/model text must never reach the bounded notes. The collector's
/// own inline test covers the behavior; this pins the source side: notes
/// interpolate only the two aggregate counts and the two fixed database
/// literals.
#[test]
fn opencode_notes_interpolate_counts_only() {
    let o = opencode_prod();
    assert!(
        o.contains("opencode: skipped {malformed} malformed local usage record(s)")
            && o.contains("opencode: skipped {unsupported} local usage record(s) from unsupported provider(s)"),
        "record notes interpolate the aggregate count only (FR-33)"
    );
    for forbidden in [
        "note.push_str",
        "eprintln",
        "println!(\"opencode",
        "writeln",
    ] {
        assert!(
            !o.contains(forbidden),
            "notes must be built only through the two bounded pushes: `{forbidden}`"
        );
    }
}

/// T6 pins the two fixed OpenCode database-failure note literals inside
/// `tui.rs::opencode_db_failed`; the collector owns those literals as
/// `NOTE_UNREADABLE` / `NOTE_INCOMPATIBLE`. This cross-file contract keeps
/// the TUI predicate byte-exact with the collector constants so a future
/// wording drift cannot silently break the `apply_local_refresh`
/// preservation invariant (FR-27/AC-5).
#[test]
fn tui_opencode_db_failed_pins_collector_note_constants_byte_exact() {
    let o = opencode_prod();
    let extract_const = |name: &str| -> String {
        let marker = format!("const {name}: &str = \"");
        let start = o
            .find(&marker)
            .unwrap_or_else(|| panic!("collector const {name} missing"));
        let rest = &o[start + marker.len()..];
        let end = rest
            .find('"')
            .unwrap_or_else(|| panic!("collector const {name} unterminated"));
        rest[..end].to_string()
    };
    let unreadable = extract_const("NOTE_UNREADABLE");
    let incompatible = extract_const("NOTE_INCOMPATIBLE");
    let tui = read("src/tui.rs");
    let predicate_start = tui
        .find("fn opencode_db_failed")
        .expect("tui must own the opencode_db_failed predicate");
    let predicate = &tui[predicate_start..];
    assert!(
        predicate.contains(&format!("\"{unreadable}\"")),
        "tui::opencode_db_failed must carry the byte-exact NOTE_UNREADABLE literal {unreadable:?}"
    );
    assert!(
        predicate.contains(&format!("\"{incompatible}\"")),
        "tui::opencode_db_failed must carry the byte-exact NOTE_INCOMPATIBLE literal {incompatible:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 9 (AC-1): one coherent WAL-backed OpenCode Qwen fixture produces
// exact provider/model/request/fresh-input/output+reasoning/cache totals
// through CLI table, JSON, CSV, and the default overview. The inline WAL
// fixtures in `src/local/opencode.rs` prove WAL visibility at the
// collector/probe level; the pre-T9 report fixtures used non-WAL databases
// — this is the missing end-to-end leg.
// ---------------------------------------------------------------------------

/// WAL-mode fixture (FR-7/AC-1): the schema is committed BEFORE the
/// journal switches to WAL, so the main file holds the schema while the
/// committed rows live only in `opencode.db-wal` frames (no checkpoint
/// runs afterward). The returned writer connection stays open for the
/// whole test — its closure would checkpoint/clean the WAL, which is
/// exactly what these fixtures must avoid.
fn write_wal_opencode_db(dir: &Path, rows: &[(i64, &str, &str)]) -> rusqlite::Connection {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("opencode.db");
    let w = rusqlite::Connection::open(&path).unwrap();
    w.execute_batch(
        "CREATE TABLE message (id TEXT PRIMARY KEY, sessionID TEXT, time_created INTEGER, \
         time_updated INTEGER, role TEXT, providerID TEXT, modelID TEXT, data TEXT)",
    )
    .unwrap();
    w.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    let mode: String = w
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal", "the fixture must run in WAL journal mode");
    for (t, id, data) in rows {
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, t, data],
        )
        .unwrap();
    }
    w
}

/// AC-1: a WAL-backed OpenCode fixture with Qwen records emits exact
/// provider/model/request/fresh-input/output+reasoning/cache-read/cache-write
/// totals through the CLI table, JSON, and CSV paths, and the default
/// overview — while a copy of the main database alone holds no rows, so
/// the totals genuinely come from committed WAL frames (FR-7). The TUI
/// leg composes with the existing deterministic TestBackend render
/// contracts in `src/tui.rs`
/// (`opencode_canonical_rows_render_local_provenance_and_keep_the_footer`,
/// `saturated_local_events_render_header_chart_and_model_paths_without_panic`)
/// plus the source contract pinning the TUI local tick to the same
/// `local::opencode::collect` entry point
/// (`opencode_tui_local_tick_collects_directly_and_preserves_snapshot_on_db_failure`);
/// a real terminal process is not appropriate for CI.
#[test]
fn opencode_wal_backed_qwen_fixture_emits_exact_cli_totals_end_to_end() {
    let sb = Sandbox::new("t9-ac1-wal");
    let dir = sb.dir.join("wal-usage");
    let t1 = ms_at("2026-08-10T10:00:00Z");
    let t2 = ms_at("2026-08-10T11:00:00Z");
    let _w = write_wal_opencode_db(
        &dir,
        &[
            (
                t1,
                "id-max",
                record("alibaba", "qwen3.8-max", t1, [1500, 800, 200, 1000, 0]).as_str(),
            ),
            (
                t2,
                "id-preview",
                record(
                    "alibaba-token-plan",
                    "qwen3.8-max-preview",
                    t2,
                    [100, 50, 10, 0, 25],
                )
                .as_str(),
            ),
        ],
    );

    // WAL-backedness proof: the -wal file carries committed frames, and a
    // copy of the main database alone holds no rows (FR-7/AC-6).
    let wal_len = fs::metadata(dir.join("opencode.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(
        wal_len > 0,
        "opencode.db-wal must exist and carry committed frames"
    );
    let main_only = dir.join("main-only.db");
    fs::copy(dir.join("opencode.db"), &main_only).unwrap();
    let ro = rusqlite::Connection::open_with_flags(
        &main_only,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let main_count: i64 = ro
        .query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        main_count, 0,
        "the main database file alone holds no rows — the fixture is genuinely WAL-only"
    );

    // JSON: exact provider/model/request/fresh-input/output+reasoning/cache totals.
    let db = dir.join("opencode.db");
    let rows = usage_rows(&sb, &db, &["--group-by", "provider,model", "--json"]);
    assert_eq!(rows.len(), 2, "both WAL-backed Qwen records emit events");
    assert_eq!(
        rows[0]["keys"],
        serde_json::json!(["2026-08-10", "qwen", "qwen3.8-max"]),
        "the first row carries period, canonical provider, and the preserved OpenCode model id"
    );
    assert_eq!(rows[0]["requests"], 1);
    assert_eq!(
        rows[0]["input_tokens"], 1500,
        "fresh input is never cache-subtracted"
    );
    assert_eq!(
        rows[0]["output_tokens"], 1000,
        "output + reasoning (800 + 200) fold into output"
    );
    assert_eq!(rows[0]["cache_read_tokens"], 1000);
    assert_eq!(rows[0]["cache_write_tokens"], 0);
    assert_eq!(rows[0]["total_tokens"], 3500);
    assert_eq!(rows[0]["tool_calls"], 0);
    assert_eq!(
        rows[1]["keys"],
        serde_json::json!(["2026-08-10", "qwen", "qwen3.8-max-preview"])
    );
    assert_eq!(rows[1]["requests"], 1);
    assert_eq!(rows[1]["input_tokens"], 100);
    assert_eq!(rows[1]["output_tokens"], 60, "50 output + 10 reasoning");
    assert_eq!(rows[1]["cache_read_tokens"], 0);
    assert_eq!(rows[1]["cache_write_tokens"], 25);
    assert_eq!(rows[1]["total_tokens"], 185);

    // CSV: exact machine output, unchanged schema, WAL-backed totals.
    let out = run_usage(&sb, &db, &["--group-by", "provider,model", "--csv"]);
    assert!(out.status.success(), "{}", sb.stderr(&out));
    assert_eq!(
        sb.stdout(&out),
        "period,provider,model,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,total_tokens,tool_calls,est_cost_usd,has_cost\n\
         2026-08-10,qwen,qwen3.8-max,1,1500,1000,1000,0,3500,0,0,false\n\
         2026-08-10,qwen,qwen3.8-max-preview,1,100,60,0,25,185,0,0,false\n",
        "the CSV keeps the exact header and carries the WAL-backed exact totals"
    );
    assert!(
        !sb.stderr(&out).contains("opencode:"),
        "a fully valid WAL-backed fixture emits no OpenCode diagnostics: {}",
        sb.stderr(&out)
    );

    // Human table: canonical provider, preserved model ids, exact totals.
    let out = run_usage(&sb, &db, &["--group-by", "provider,model"]);
    assert!(out.status.success(), "{}", sb.stderr(&out));
    let stdout = sb.stdout(&out);
    for needle in [
        "qwen",
        "qwen3.8-max",
        "qwen3.8-max-preview",
        "1,500",
        "1,000",
        "3,500",
        "TOTAL",
    ] {
        assert!(
            stdout.contains(needle),
            "the WAL-backed table must render `{needle}`:\n{stdout}"
        );
    }

    // Default overview: the bare `llmu` landing view over a WAL-backed DB.
    // The overview window is inherently now-relative (the same accepted
    // exception as `opencode_overview_shows_local_provenance_and_saturates_without_panic`).
    let ov_dir = sb.dir.join("wal-overview");
    let t_now = chrono::Utc::now().timestamp_millis() - 2 * 60 * 60 * 1000;
    let _w2 = write_wal_opencode_db(
        &ov_dir,
        &[(
            t_now,
            "id-ov",
            record("alibaba", "qwen-max", t_now, [10, 20, 5, 3, 2]).as_str(),
        )],
    );
    let out = sb
        .cmd(Some(&ov_dir), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap()])
        .output()
        .expect("spawning llmu binary");
    assert!(
        out.status.success(),
        "the overview must not fail: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    assert!(
        stdout.contains("usage counted from: qwen (local logs)"),
        "the overview scope line must show the WAL-backed Qwen local feed:\n{stdout}"
    );
    assert!(
        stdout.contains("TOTAL") && stdout.contains("usage counted from"),
        "the overview table must render the WAL-backed provider row:\n{stdout}"
    );
}

/// FR-30/NFR-3 black-box: a busy (exclusively locked) database prints
/// `opencode no` within the 250 ms bound plus CI tolerance, and the status
/// path never surfaces a diagnostic on stderr.
#[test]
fn providers_opencode_busy_db_answers_no_bounded_without_diagnostics() {
    let sb = Sandbox::new("t7-busy");
    let dir = sb.dir.join("busy-data");
    write_opencode_db(&dir, &[(1000, "m1", &eligible_message("openai"))]);
    let lock = rusqlite::Connection::open(dir.join("opencode.db")).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let start = Instant::now();
    let out = sb
        .cmd(Some(&dir), Some(&sb.data()))
        .args(["--config", sb.config_path().to_str().unwrap(), "providers"])
        .output()
        .expect("spawning llmu binary");
    let elapsed = start.elapsed();
    drop(lock);
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let row = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("opencode"))
        .expect("providers must list an opencode row");
    assert!(row.contains("no"), "a busy DB answers no:\n{row}");
    assert!(
        elapsed >= Duration::from_millis(150) && elapsed < Duration::from_secs(2),
        "the busy bound (250 ms) plus CI tolerance must hold: {elapsed:?}"
    );
    assert!(
        !sb.stderr(&out).contains("note:"),
        "status never surfaces diagnostics on a busy DB: {}",
        sb.stderr(&out)
    );
}
