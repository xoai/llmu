//! TUI token-display wiring contract. Std-only source assertions mirror
//! the project's other integration tests without importing private modules.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    fs::read_to_string(repo_root().join(rel)).expect("reading repo file")
}

#[test]
fn tui_labels_cache_and_compacts_cache_scale_totals() {
    let report = read("src/report.rs");
    assert!(report.contains("pub fn fmt_compact"));
    assert!(report.contains("pub fn totals_summary"));
    assert!(report.contains("/ cache {})"));

    let tui = read("src/tui.rs");
    assert!(tui.contains("report::totals_summary(&g)"));
    assert!(tui.contains("report::fmt_compact(t.total_tokens())"));
}

#[test]
fn integration_test_never_imports_private_llmu_modules() {
    let src = include_str!("token_display.rs");
    let forbidden = ["use ", "llmu", "::"].concat();
    assert!(!src.contains(&forbidden));
}
