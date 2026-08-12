//! Task 1: hermetic Qwen configuration and settings discovery contracts
//! (RED -> GREEN).
//!
//! Std-only, never importing private `llmu` modules. Mirrors the
//! `tests/http_cache.rs` / `tests/balance_history.rs` pattern: source
//! contracts pin the FR-1 / FR-2 surface textually, and black-box tests
//! drive the real binary (`CARGO_BIN_EXE_llmu`) against isolated
//! HOME/XDG directories with every provider env var removed, so no real
//! credential, real-home Qwen discovery, or network ever participates.
//!
//! The black-box `discover::apply` integration test pins Qwen home and
//! runtime through explicit `[qwen]` temp-directory overrides, leaves the
//! test process environment untouched (child env is set only via
//! `Command::env`/`env_remove`), reads a temp `settings.json`, and proves
//! the resolved fields plus provenance are installed.

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
// Source contracts (FR-1, FR-2): the shape the binary must expose.
// ---------------------------------------------------------------------------

#[test]
fn config_qwen_section_declares_all_override_fields() {
    let cfg = read("src/config.rs");
    for needle in [
        "pub struct QwenCfg",
        "pub qwen: QwenCfg",
        "pub standard_key: Option<String>",
        "pub coding_plan_key: Option<String>",
        "pub token_plan_key: Option<String>",
        "pub home: Option<PathBuf>",
        "pub runtime_dir: Option<PathBuf>",
    ] {
        assert!(
            cfg.contains(needle),
            "src/config.rs must declare `{needle}` (FR-1 `[qwen]` section)"
        );
    }
}

#[test]
fn config_qwen_uses_exact_environment_names() {
    let cfg = read("src/config.rs");
    for needle in [
        "DASHSCOPE_API_KEY",
        "BAILIAN_API_KEY",
        "BAILIAN_CODING_PLAN_API_KEY",
        "BAILIAN_TOKEN_PLAN_API_KEY",
        "QWEN_HOME",
        "QWEN_RUNTIME_DIR",
    ] {
        assert!(
            cfg.contains(needle),
            "src/config.rs must consult `{needle}` (FR-1 exact env names)"
        );
    }
}

#[test]
fn config_qwen_parses_typed_settings_values() {
    let cfg = read("src/config.rs");
    for needle in [
        "pub struct QwenSettings",
        "pub env: HashMap<String, String>",
        "runtimeOutputDir",
    ] {
        assert!(
            cfg.contains(needle),
            "src/config.rs must expose `{needle}` for typed Qwen settings (FR-2)"
        );
    }
}

#[test]
fn plan_class_is_never_inferred_from_sk_sp_prefix() {
    let cfg = read("src/config.rs");
    assert!(
        !cfg.contains("starts_with(\"sk-sp"),
        "Coding Plan / Token Plan classes must never be inferred from an `sk-sp-*` prefix alone (FR-1)"
    );
}

#[test]
fn discover_applies_qwen_settings_with_provenance() {
    let d = read("src/discover.rs");
    for needle in [
        "fn read_qwen_settings",
        "fn apply_qwen",
        "apply_qwen(cfg",
        "settings.json",
        "qwen.home",
        "qwen.standard_key",
        "qwen.coding_plan_key",
        "qwen.token_plan_key",
        "qwen.runtime_dir",
    ] {
        assert!(
            d.contains(needle),
            "src/discover.rs must `{needle}` (FR-2 read-only settings discovery)"
        );
    }
    assert!(
        d.contains("pub fn apply("),
        "src/discover.rs keeps the existing `apply` entry point"
    );
}

#[test]
fn integration_tests_never_import_llmu() {
    let me = read("tests/qwen_config.rs");
    let use_llmu = "use ".to_owned() + "llmu";
    let module_path = "llmu".to_owned() + "::";
    assert!(
        !me.contains(&use_llmu) && !me.contains(&module_path),
        "tests/qwen_config.rs is a std-only contract and must not import llmu"
    );
}

// ---------------------------------------------------------------------------
// Black-box discover::apply integration (hermetic, no real-home access).
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
            "llmu-qwen-config-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::create_dir_all(dir.join("config")).unwrap();
        fs::create_dir_all(dir.join("data")).unwrap();
        fs::create_dir_all(dir.join("qwen-home")).unwrap();
        Sandbox { dir }
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    /// Explicit `[qwen] home` override pinned to the sandbox.
    fn qwen_home(&self) -> PathBuf {
        self.dir.join("qwen-home")
    }

    /// Explicit `[qwen] runtime_dir` override pinned to the sandbox.
    fn qwen_runtime(&self) -> PathBuf {
        self.dir.join("qwen-runtime")
    }

    fn write_config(&self, body: &str) {
        fs::write(self.config_path(), body).unwrap();
    }

    fn write_settings(&self, body: &str) {
        fs::write(self.qwen_home().join("settings.json"), body).unwrap();
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

/// AC-2/AC-10 hermetic `discover::apply` integration: explicit `[qwen]`
/// home/runtime temp-directory overrides pin every fallback, a temp
/// `settings.json` supplies the three credential classes, and
/// `llmu providers` proves the resolved fields plus settings-path
/// provenance are installed without invoking real-home Qwen discovery.
#[test]
fn discover_apply_pins_qwen_home_reads_temp_settings_and_records_provenance() {
    let sb = Sandbox::new("settings");
    sb.write_config(&format!(
        "[qwen]\nhome = \"{}\"\nruntime_dir = \"{}\"\n",
        sb.qwen_home().display(),
        sb.qwen_runtime().display()
    ));
    sb.write_settings(
        r#"{"env":{"DASHSCOPE_API_KEY":"sk-std","BAILIAN_CODING_PLAN_API_KEY":"sk-sp-coding","BAILIAN_TOKEN_PLAN_API_KEY":"sk-sp-token"},"advanced":{"runtimeOutputDir":"./out"}}"#,
    );
    let out = sb.run(&["--config", sb.config_path().to_str().unwrap(), "providers"]);
    assert!(
        out.status.success(),
        "providers must succeed: {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    let settings_path = sb.qwen_home().join("settings.json").display().to_string();
    for field in [
        "qwen.standard_key",
        "qwen.coding_plan_key",
        "qwen.token_plan_key",
    ] {
        assert!(
            stdout.contains(field),
            "missing {field} in providers output (FR-1):\n{stdout}"
        );
    }
    assert_eq!(
        stdout.matches(&settings_path).count(),
        3,
        "all three Qwen keys must name the settings path in provenance (FR-2):\n{stdout}"
    );
    for env_name in [
        "env DASHSCOPE_API_KEY",
        "env BAILIAN_CODING_PLAN_API_KEY",
        "env BAILIAN_TOKEN_PLAN_API_KEY",
    ] {
        assert!(
            !stdout.contains(env_name),
            "no env provenance may appear when the child env removed every Qwen variable:\n{stdout}"
        );
    }
}

/// AC-10: a malformed settings file must not break other provider loading
/// and must not populate any Qwen field or leak a value.
#[test]
fn malformed_qwen_settings_do_not_break_other_providers() {
    let sb = Sandbox::new("malformed");
    sb.write_config(&format!(
        "[qwen]\nhome = \"{}\"\n",
        sb.qwen_home().display()
    ));
    sb.write_settings("not json {{{");
    let out = sb.run(&["--config", sb.config_path().to_str().unwrap(), "providers"]);
    assert!(
        out.status.success(),
        "malformed Qwen settings must not break other providers (AC-10): {}",
        sb.stderr(&out)
    );
    let stdout = sb.stdout(&out);
    assert!(
        stdout.contains("capabilities"),
        "the providers table must still render (AC-10):\n{stdout}"
    );
    assert!(
        !stdout.contains("qwen."),
        "no Qwen field may be populated from malformed settings (AC-10):\n{stdout}"
    );
    // AC-7: the provider table lists `qwen` unconditionally, so the
    // malformed-settings isolation is proven by the absence of any
    // populated `qwen.*` field/provenance line, not by the row itself.
    assert!(
        stdout.contains("qwen "),
        "the registered qwen provider row must still render (AC-7):\n{stdout}"
    );
}
