//! Task 1: failing release-automation contract tests.
//!
//! These integration tests pin the release contract from the approved spec
//! (FR-1..FR-4) and plan before any implementation exists:
//!
//! * `scripts/finalize_changelog.py` — a Python-stdlib CLI with `finalize`
//!   (`--changelog` + `--cargo` paths, deterministic `--date`) and
//!   `extract --version`; exercised through the real `python3` interpreter
//!   in process/time-unique temp directories.
//! * `.github/workflows/release.yml` — four jobs (release-please,
//!   resolve-release, build, publish), default-branch trigger, manual
//!   recovery input, exact five-target matrix, aggregate draft publication.
//! * `release-please-config.json` / `.release-please-manifest.json` —
//!   skip-changelog, draft, force-tag-creation, root package at 0.1.0.
//! * `README.md` — release-PR lifecycle documentation.
//!
//! Everything is std-only. Tests must fail because the finalizer / config /
//! workflow / documentation contracts are missing or stale — never because
//! of malformed Rust.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const TARGET_TRIPLES: [&str; 5] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];
const EXPECTED_JOBS: [&str; 4] = ["release-please", "resolve-release", "build", "publish"];

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn finalizer_script() -> PathBuf {
    repo_root().join("scripts").join("finalize_changelog.py")
}

fn workflow_path() -> PathBuf {
    repo_root()
        .join(".github")
        .join("workflows")
        .join("release.yml")
}

fn config_path() -> PathBuf {
    repo_root().join("release-please-config.json")
}

fn manifest_path() -> PathBuf {
    repo_root().join(".release-please-manifest.json")
}

fn readme_path() -> PathBuf {
    repo_root().join("README.md")
}

fn read_named(path: &Path, what: &str) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{what} missing or unreadable at {}: {e}", path.display()))
}

fn workflow_text() -> String {
    read_named(&workflow_path(), "release workflow")
}

fn config_text() -> String {
    read_named(&config_path(), "release-please-config.json")
}

fn manifest_text() -> String {
    read_named(&manifest_path(), ".release-please-manifest.json")
}

fn readme_text() -> String {
    read_named(&readme_path(), "README.md")
}

/// Process/time-unique temp directory, removed on drop (panic-safe cleanup).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "llmu-release-test-{}-{n}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp fixture dir");
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_fixture(dir: &TempDir, name: &str, content: &str) -> PathBuf {
    let p = dir.path().join(name);
    fs::write(&p, content).unwrap_or_else(|e| panic!("write fixture {}: {e}", p.display()));
    p
}

fn cargo_fixture(version: &str) -> String {
    format!("[package]\nname = \"llmu\"\nversion = \"{version}\"\n")
}

fn changelog_fixture() -> String {
    "# Changelog\n\nAll notable changes to this project will be documented in this file.\n\n## [Unreleased]\n\n### Added\n\n- New feature.\n\n## [0.1.0] - 2026-01-01\n\n### Fixed\n\n- Old fix.\n"
        .to_string()
}

fn run_finalizer(args: &[&str], cwd: &Path) -> Output {
    let script = finalizer_script();
    assert!(
        script.exists(),
        "finalizer script missing at {} — Task 2 must implement `scripts/finalize_changelog.py`",
        script.display()
    );
    Command::new("python3")
        .arg(&script)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", script.display()))
}

fn stdout_of(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr_of(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Split a changelog into `(heading, body)` pairs for `## ` level-2 headings.
fn sections(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut body = String::new();
    for line in text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            if let Some(h) = cur.take() {
                out.push((h, std::mem::take(&mut body)));
            }
            cur = Some(heading.to_string());
        } else if cur.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(h) = cur.take() {
        out.push((h, body));
    }
    out
}

fn body_of<'a>(secs: &'a [(String, String)], heading: &str) -> Option<&'a String> {
    secs.iter().find(|(h, _)| h == heading).map(|(_, b)| b)
}

fn count(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

fn skip_ws(s: &str) -> &str {
    let i = s.find(|c: char| !c.is_whitespace()).unwrap_or(s.len());
    &s[i..]
}

/// True if `"key" : <value>` appears anywhere in a JSON document, for
/// either a quoted or a bare (true/false/number) value.
fn json_key_value(text: &str, key: &str, value: &str) -> bool {
    let needle = format!("\"{key}\"");
    let mut from = 0;
    while let Some(rel) = text[from..].find(&needle) {
        let idx = from + rel;
        let mut rest = skip_ws(&text[idx + needle.len()..]);
        if let Some(r) = rest.strip_prefix(':') {
            rest = skip_ws(r);
            if let Some(r2) = rest.strip_prefix('"') {
                if let Some(end) = r2.find('"') {
                    if &r2[..end] == value {
                        return true;
                    }
                }
            } else if let Some(after) = rest.strip_prefix(value) {
                let ok = after.is_empty()
                    || matches!(after.chars().next(), Some(c) if !(c.is_alphanumeric() || c == '_' || c == '-'));
                if ok {
                    return true;
                }
            }
        }
        from = idx + 1;
    }
    false
}

/// Exactly two-space-indented `key:` lines at the `jobs:` level.
/// Comment-only YAML lines are never keys.
fn is_top_level_key(line: &str) -> bool {
    if line.trim_start().starts_with('#') {
        return false;
    }
    if !line.starts_with("  ") || line.starts_with("   ") {
        return false;
    }
    let rest = &line[2..];
    match rest.find(':') {
        Some(i) => {
            !rest[..i].is_empty()
                && rest[..i]
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        }
        None => false,
    }
}

fn job_ids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_jobs = false;
    for line in text.lines() {
        if line == "jobs:" {
            in_jobs = true;
            continue;
        }
        if in_jobs {
            if is_top_level_key(line) {
                out.push(line[2..].trim_end_matches(':').to_string());
            } else if line.trim_start().starts_with('#') {
                // comment-only YAML lines carry no structure
                continue;
            } else if !line.starts_with(' ') && !line.is_empty() {
                break;
            }
        }
    }
    out
}

/// Slice of `text` from the exactly-two-space-indented `key:` line up to the
/// next exactly-two-space-indented key (or end of file).
fn indented_block(text: &str, key: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let marker = format!("  {key}:");
    let start = lines.iter().position(|l| *l == marker.as_str())?;
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        if is_top_level_key(l) {
            end = i;
            break;
        }
    }
    Some(lines[start..end].join("\n"))
}

fn job_block(text: &str, job: &str) -> Option<String> {
    indented_block(text, job)
}

/// Slice of `text` from a column-0 `key:` line up to the next column-0
/// key (or end of file); comment-only lines are skipped.
fn top_level_block(text: &str, key: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let marker = key.to_string() + ":";
    let start = lines.iter().position(|l| *l == marker.as_str())?;
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        if l.trim_start().starts_with('#') {
            continue;
        }
        if !l.starts_with(' ') && !l.trim().is_empty() {
            end = i;
            break;
        }
    }
    Some(lines[start..end].join("\n"))
}

/// Every `run:` step block (the `run:` line plus its indented body), in
/// document order, for shell-interpolation contract checks.
fn run_blocks(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("        run:") {
            let mut block = String::from(lines[i]);
            block.push('\n');
            let mut j = i + 1;
            while j < lines.len()
                && (lines[j].starts_with("          ") || lines[j].trim().is_empty())
            {
                block.push_str(lines[j]);
                block.push('\n');
                j += 1;
            }
            out.push(block);
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// True if the JSON document has `"packages" : { "." : { ...` — the root
/// package `.` declared as an object inside the `packages` map.
fn json_root_package_object(text: &str) -> bool {
    let needle = "\"packages\"";
    let mut from = 0;
    while let Some(rel) = text[from..].find(needle) {
        let idx = from + rel;
        let mut rest = skip_ws(&text[idx + needle.len()..]);
        if let Some(r) = rest.strip_prefix(':') {
            rest = skip_ws(r);
            if let Some(r) = rest.strip_prefix('{') {
                rest = skip_ws(r);
                if let Some(r) = rest.strip_prefix("\".\"") {
                    rest = skip_ws(r);
                    if let Some(r) = rest.strip_prefix(':') {
                        if skip_ws(r).starts_with('{') {
                            return true;
                        }
                    }
                }
            }
        }
        from = idx + 1;
    }
    false
}

/// Every `uses:` line as `(owner/repo, ref)`.
fn uses_refs(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix("uses:") {
            let rest = rest.trim();
            if let Some(at) = rest.rfind('@') {
                out.push((rest[..at].to_string(), rest[at + 1..].to_string()));
            }
        }
    }
    out
}

fn line_of(text: &str, needle: &str) -> Option<usize> {
    text.lines().position(|l| l.contains(needle))
}

// ---------------------------------------------------------------------------
// finalizer behavior contract (FR-1)
// ---------------------------------------------------------------------------

/// `finalize --changelog <f> --cargo <c> [--date YYYY-MM-DD]` renames the
/// non-empty `## [Unreleased]` notes into exactly one dated version section
/// and recreates one empty `## [Unreleased]` above it.
#[test]
fn finalize_normal_unreleased_to_versioned() {
    let tmp = TempDir::new();
    let changelog = write_fixture(&tmp, "CHANGELOG.md", &changelog_fixture());
    let cargo = write_fixture(&tmp, "Cargo.toml", &cargo_fixture("0.2.0"));
    let old = fs::read_to_string(&changelog).expect("read fixture changelog");

    let out = run_finalizer(
        &[
            "finalize",
            "--changelog",
            changelog.to_str().expect("utf8 path"),
            "--cargo",
            cargo.to_str().expect("utf8 path"),
            "--date",
            "2026-08-11",
        ],
        &repo_root(),
    );
    assert!(
        out.status.success(),
        "finalize must succeed on a normal Unreleased section; stderr: {}",
        stderr_of(&out)
    );

    let new = fs::read_to_string(&changelog).expect("read finalized changelog");
    assert_eq!(
        count(&new, "## [0.2.0] - 2026-08-11"),
        1,
        "finalize must produce exactly one `## [0.2.0] - 2026-08-11` heading\n{new}"
    );
    assert_eq!(
        count(&new, "## [Unreleased]"),
        1,
        "finalize must recreate exactly one `## [Unreleased]` heading\n{new}"
    );
    let secs = sections(&new);
    let unreleased_idx = secs
        .iter()
        .position(|(h, _)| h == "[Unreleased]")
        .expect("fresh Unreleased section");
    assert!(
        secs[unreleased_idx].1.trim().is_empty(),
        "the fresh Unreleased section must be empty, got:\n{}",
        secs[unreleased_idx].1
    );
    assert_eq!(
        secs.get(unreleased_idx + 1).map(|(h, _)| h.as_str()),
        Some("[0.2.0] - 2026-08-11"),
        "the fresh empty Unreleased heading must sit directly above `## [0.2.0] - 2026-08-11` with nothing between them"
    );
    let versioned = body_of(&secs, "[0.2.0] - 2026-08-11").expect("0.2.0 section");
    assert!(
        versioned.contains("- New feature.") && versioned.contains("### Added"),
        "the Unreleased notes must move into the versioned section, got:\n{versioned}"
    );
    let older = body_of(&secs, "[0.1.0] - 2026-01-01").expect("0.1.0 section");
    assert!(
        older.contains("- Old fix."),
        "older section content must be untouched, got:\n{older}"
    );

    let old_i = old.find("## [Unreleased]").expect("fixture Unreleased");
    let new_i = new.find("## [Unreleased]").expect("finalized Unreleased");
    assert_eq!(
        &old[..old_i],
        &new[..new_i],
        "introductory text before the first heading must be preserved byte-for-byte"
    );
    let old_t = old.find("## [0.1.0]").expect("fixture 0.1.0");
    let new_t = new.find("## [0.1.0]").expect("finalized 0.1.0");
    assert_eq!(
        &old[old_t..],
        &new[new_t..],
        "older sections must be preserved byte-for-byte"
    );
}

/// Re-running `finalize` on the same working copy is a byte-level no-op and
/// preserves the original release date even when a different `--date` is
/// passed or no date is passed at all.
#[test]
fn finalize_rerun_is_noop_and_preserves_date() {
    let tmp = TempDir::new();
    let changelog = write_fixture(&tmp, "CHANGELOG.md", &changelog_fixture());
    let cargo = write_fixture(&tmp, "Cargo.toml", &cargo_fixture("0.2.0"));
    let c = changelog.to_str().expect("utf8 path");
    let g = cargo.to_str().expect("utf8 path");

    let first = run_finalizer(
        &[
            "finalize",
            "--changelog",
            c,
            "--cargo",
            g,
            "--date",
            "2026-08-11",
        ],
        &repo_root(),
    );
    assert!(
        first.status.success(),
        "first finalize failed: {}",
        stderr_of(&first)
    );
    let snapshot = fs::read_to_string(&changelog).expect("read first result");

    let second = run_finalizer(
        &[
            "finalize",
            "--changelog",
            c,
            "--cargo",
            g,
            "--date",
            "2099-01-01",
        ],
        &repo_root(),
    );
    assert!(
        second.status.success(),
        "re-finalize failed: {}",
        stderr_of(&second)
    );
    let again = fs::read_to_string(&changelog).expect("read second result");
    assert_eq!(
        snapshot, again,
        "re-finalizing an already-finalized working copy must be a byte-level no-op"
    );
    assert!(
        again.contains("## [0.2.0] - 2026-08-11"),
        "the original release date must be preserved\n{again}"
    );
    assert!(
        !again.contains("2099-01-01"),
        "a fresh --date must not overwrite the original release date\n{again}"
    );

    let third = run_finalizer(&["finalize", "--changelog", c, "--cargo", g], &repo_root());
    assert!(
        third.status.success(),
        "date-less re-finalize failed: {}",
        stderr_of(&third)
    );
    assert_eq!(
        snapshot,
        fs::read_to_string(&changelog).expect("read third result"),
        "re-finalizing without --date must also be a byte-level no-op"
    );
}

/// Intro text and every older section survive finalization byte-for-byte,
/// including surrounding whitespace and multiple pre-release sections.
#[test]
fn finalize_preserves_intro_and_older_sections_bytes() {
    let fixture = "# Changelog\n\nAll notable changes to this project will be documented in this file.\n\nThe date format follows [ISO 8601](https://en.wikipedia.org/wiki/ISO_8601).\n\n## [Unreleased]\n\n### Added\n\n- Support for the release-automation flow.\n\n## [0.9.0] - 2025-11-30\n\n### Changed\n\n- Reworked the quota gauges.\n\n## [0.8.0] - 2025-06-01\n\n### Removed\n\n- Removed the legacy import path.\n";
    let tmp = TempDir::new();
    let changelog = write_fixture(&tmp, "CHANGELOG.md", fixture);
    let cargo = write_fixture(&tmp, "Cargo.toml", &cargo_fixture("0.2.0"));

    let out = run_finalizer(
        &[
            "finalize",
            "--changelog",
            changelog.to_str().expect("utf8 path"),
            "--cargo",
            cargo.to_str().expect("utf8 path"),
            "--date",
            "2026-08-11",
        ],
        &repo_root(),
    );
    assert!(out.status.success(), "finalize failed: {}", stderr_of(&out));

    let new = fs::read_to_string(&changelog).expect("read finalized changelog");
    let old_i = fixture.find("## [Unreleased]").expect("fixture Unreleased");
    let new_i = new.find("## [Unreleased]").expect("finalized Unreleased");
    assert_eq!(
        &fixture[..old_i],
        &new[..new_i],
        "intro paragraph must be preserved byte-for-byte"
    );
    let old_t = fixture.find("## [0.9.0]").expect("fixture 0.9.0");
    let new_t = new.find("## [0.9.0]").expect("finalized 0.9.0");
    assert_eq!(
        &fixture[old_t..],
        &new[new_t..],
        "the 0.9.0 and 0.8.0 sections must be preserved byte-for-byte"
    );
    assert_eq!(
        count(&new, "## [Unreleased]"),
        1,
        "exactly one Unreleased heading\n{new}"
    );
    assert_eq!(
        count(&new, "## [0.2.0] - 2026-08-11"),
        1,
        "exactly one dated version heading\n{new}"
    );
    assert!(sections(&new)
        .iter()
        .any(|(h, b)| h == "[0.2.0] - 2026-08-11" && b.contains("release-automation flow")));
}

/// Finalization fails loudly when there is no Unreleased section at all.
#[test]
fn finalize_fails_when_unreleased_missing() {
    let fixture = "# Changelog\n\nAll notable changes to this project will be documented in this file.\n\n## [0.1.0] - 2026-01-01\n\n### Fixed\n\n- Old fix.\n";
    let tmp = TempDir::new();
    let changelog = write_fixture(&tmp, "CHANGELOG.md", fixture);
    let cargo = write_fixture(&tmp, "Cargo.toml", &cargo_fixture("0.2.0"));

    let out = run_finalizer(
        &[
            "finalize",
            "--changelog",
            changelog.to_str().expect("utf8 path"),
            "--cargo",
            cargo.to_str().expect("utf8 path"),
            "--date",
            "2026-08-11",
        ],
        &repo_root(),
    );
    assert!(
        !out.status.success(),
        "finalize must fail when `## [Unreleased]` is missing entirely"
    );
    assert!(
        !stderr_of(&out).trim().is_empty(),
        "missing-Unreleased failure must be loud (non-empty stderr)"
    );
}

/// Finalization fails loudly when the Unreleased section contains no notes.
#[test]
fn finalize_fails_when_unreleased_empty() {
    let fixture = "# Changelog\n\nAll notable changes to this project will be documented in this file.\n\n## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n\n### Fixed\n\n- Old fix.\n";
    let tmp = TempDir::new();
    let changelog = write_fixture(&tmp, "CHANGELOG.md", fixture);
    let cargo = write_fixture(&tmp, "Cargo.toml", &cargo_fixture("0.2.0"));

    let out = run_finalizer(
        &[
            "finalize",
            "--changelog",
            changelog.to_str().expect("utf8 path"),
            "--cargo",
            cargo.to_str().expect("utf8 path"),
            "--date",
            "2026-08-11",
        ],
        &repo_root(),
    );
    assert!(
        !out.status.success(),
        "finalize must fail when `## [Unreleased]` contains no release notes"
    );
    assert!(
        !stderr_of(&out).trim().is_empty(),
        "empty-Unreleased failure must be loud (non-empty stderr)"
    );
}

/// `extract --version X.Y.Z` emits exactly the requested version section —
/// heading, date, and notes — and nothing else.
#[test]
fn extract_emits_exactly_one_version_section() {
    let tmp = TempDir::new();
    let changelog = write_fixture(&tmp, "CHANGELOG.md", &changelog_fixture());
    let cargo = write_fixture(&tmp, "Cargo.toml", &cargo_fixture("0.2.0"));

    let fin = run_finalizer(
        &[
            "finalize",
            "--changelog",
            changelog.to_str().expect("utf8 path"),
            "--cargo",
            cargo.to_str().expect("utf8 path"),
            "--date",
            "2026-08-11",
        ],
        &repo_root(),
    );
    assert!(fin.status.success(), "finalize failed: {}", stderr_of(&fin));

    let out = run_finalizer(
        &[
            "extract",
            "--version",
            "0.2.0",
            "--changelog",
            changelog.to_str().expect("utf8 path"),
        ],
        &repo_root(),
    );
    assert!(out.status.success(), "extract failed: {}", stderr_of(&out));
    let s = stdout_of(&out);
    assert!(
        s.contains("## [0.2.0] - 2026-08-11"),
        "extracted section must carry the dated heading, got:\n{s}"
    );
    assert_eq!(
        count(&s, "## [0.2.0]"),
        1,
        "exactly one version section:\n{s}"
    );
    assert!(
        s.contains("- New feature.") && s.contains("### Added"),
        "extracted section must contain the version's notes, got:\n{s}"
    );
    assert!(
        !s.contains("## [Unreleased]") && !s.contains("[0.1.0]") && !s.contains("# Changelog"),
        "extract must emit only the requested version section, got:\n{s}"
    );
}

/// `extract --version` of a version that has no section fails loudly.
#[test]
fn extract_fails_for_missing_version() {
    let tmp = TempDir::new();
    let changelog = write_fixture(&tmp, "CHANGELOG.md", &changelog_fixture());

    let out = run_finalizer(
        &[
            "extract",
            "--version",
            "9.9.9",
            "--changelog",
            changelog.to_str().expect("utf8 path"),
        ],
        &repo_root(),
    );
    assert!(
        !out.status.success(),
        "extract must fail when the requested version has no section"
    );
    assert!(
        !stderr_of(&out).trim().is_empty(),
        "missing-version failure must be loud (non-empty stderr)"
    );
}

// ---------------------------------------------------------------------------
// workflow contract (FR-2..FR-4)
// ---------------------------------------------------------------------------

/// The workflow defines exactly the four job contracts: release-please,
/// resolve-release, build, publish.
#[test]
fn workflow_has_exactly_four_jobs() {
    let wf = workflow_text();
    let ids = job_ids(&wf);
    assert_eq!(
        ids.len(),
        EXPECTED_JOBS.len(),
        "workflow must define exactly the four job contracts (release-please, resolve-release, build, publish); found: {ids:?}"
    );
    for id in EXPECTED_JOBS {
        assert!(ids.iter().any(|x| x == id), "missing job `{id}` in {ids:?}");
    }
}

/// The trigger is a push to the default branch plus manual recovery
/// dispatch; the legacy `push.tags: ["v*"]` trigger is removed.
#[test]
fn workflow_trigger_is_default_branch_without_tag_push() {
    let wf = workflow_text();
    assert!(
        !wf.contains("tags:"),
        "legacy tag-push trigger (`push: tags:`) must be removed"
    );
    assert!(wf.contains("push:"), "workflow must trigger on pushes");
    assert!(
        wf.contains("branches:") && wf.contains("main"),
        "push trigger must target the default branch (main)"
    );
}

/// Manual recovery dispatch exposes a required `tag` input.
#[test]
fn workflow_dispatch_has_required_tag_input() {
    let wf = workflow_text();
    let dispatch = indented_block(&wf, "workflow_dispatch")
        .unwrap_or_else(|| panic!("workflow_dispatch must be declared in the `on:` trigger block"));
    assert!(
        dispatch.contains("inputs:"),
        "workflow_dispatch must declare `inputs:`"
    );
    assert!(
        dispatch.contains("tag:"),
        "workflow_dispatch must expose an input named `tag`"
    );
    assert!(
        dispatch.contains("required: true"),
        "the manual `tag` input must be required"
    );
}

/// release-please is push-only (never runs during manual recovery),
/// exposes `release_created` / `tag_name`, discovers `prs[0].headBranchName`,
/// reruns the finalizer on that branch, and commits as github-actions[bot].
#[test]
fn release_please_job_is_push_only_with_pr_head_discovery() {
    let rp = job_block(&workflow_text(), "release-please").expect("missing job `release-please`");
    assert!(
        rp.contains("if:"),
        "release-please must carry a job-level guard"
    );
    assert!(
        rp.contains("event_name") && rp.contains("push"),
        "release-please must be push-only (guard on event_name == push)"
    );
    assert!(
        !rp.contains("always()"),
        "release-please must never run during manual recovery (no `if: always()`)"
    );
    assert!(
        rp.contains("outputs:"),
        "release-please must declare outputs"
    );
    assert!(
        rp.contains("release_created") && rp.contains("tag_name"),
        "release-please must expose `release_created` and `tag_name` outputs"
    );
    assert!(
        rp.contains("prs_created"),
        "release-please must gate the PR-head work on `prs_created`"
    );
    assert!(
        rp.contains("outputs.prs"),
        "release-please must consume its `prs` output to detect new/updated release PRs"
    );
    assert!(
        rp.contains("headBranchName"),
        "release-please must read `prs[0].headBranchName`"
    );
    assert!(
        rp.contains("ref: ${{") && rp.contains("headBranchName"),
        "the reconstructed PR head branch must be checked out after Release Please updates it"
    );
    assert!(
        rp.contains("finalize_changelog.py"),
        "release-please must rerun the changelog finalizer on the PR branch"
    );
    assert!(
        rp.contains("github-actions[bot]"),
        "the finalizer commit must be authored as github-actions[bot]"
    );
}

/// resolve-release needs release-please, runs with `if: always()` so manual
/// recovery works, selects Release Please's `tag_name` on automatic runs
/// (gated on `release_created`) or validates the manual `tag` input (strict
/// regex + git-level lookup only, read-only — never the GitHub Release API),
/// and emits exactly one resolved `tag` output.
#[test]
fn resolve_release_job_selects_or_validates_tag() {
    let rr = job_block(&workflow_text(), "resolve-release").expect("missing job `resolve-release`");
    assert!(
        rr.contains("needs:") && rr.contains("release-please"),
        "resolve-release must depend on release-please"
    );
    assert!(
        rr.contains("always()"),
        "resolve-release must run with `if: always()` to also serve manual recovery"
    );
    assert!(
        rr.contains("outputs:") && rr.contains("tag:"),
        "resolve-release must emit exactly one resolved `tag` output"
    );
    assert!(
        rr.contains("tag_name"),
        "automatic runs must resolve Release Please's `tag_name` output"
    );
    assert!(
        rr.contains("release_created"),
        "automatic runs must proceed only when `release_created` is true"
    );
    assert!(
        rr.contains("inputs.tag"),
        "manual runs must read the workflow_dispatch `tag` input"
    );
    assert!(
        rr.contains("^v[0-9]+\\.[0-9]+\\.[0-9]+$"),
        "manual tag must pass the strict `^v[0-9]+\\.[0-9]+\\.[0-9]+$` format check"
    );
    assert!(
        !rr.contains("gh release view"),
        "resolve-release must not look up the GitHub Release: a draft lookup needs write scope, and the publish job preflights `gh release view` with its write-scoped token"
    );
    assert!(
        rr.contains("git rev-parse") || rr.contains("git ls-remote") || rr.contains("git tag"),
        "manual tag must additionally be validated by a git-level tag lookup (`git rev-parse` / `git ls-remote`) before dispatch"
    );
    assert!(
        rr.contains("contents: read") && !rr.contains("contents: write"),
        "resolve-release must run with read-only contents permission (`contents: read`, never write)"
    );
}

/// build needs resolve-release, guards on a non-empty resolved tag, checks
/// out that exact tag, and has read-only contents permission.
#[test]
fn build_job_needs_resolve_release_and_guards_tag() {
    let b = job_block(&workflow_text(), "build").expect("missing job `build`");
    assert!(
        b.contains("needs:") && b.contains("resolve-release"),
        "build must depend on resolve-release"
    );
    assert!(
        b.contains("needs.resolve-release.outputs.tag"),
        "build must consume the resolved tag via `needs.resolve-release.outputs.tag`"
    );
    assert!(
        b.contains("!= ''"),
        "build must explicitly guard on a non-empty resolved tag before matrix dispatch"
    );
    assert!(
        b.contains("always()") && b.contains("needs.resolve-release.result == 'success'"),
        "build must override the intentionally skipped release-please dependency chain during manual recovery while still requiring resolve-release to succeed"
    );
    assert!(
        b.contains("ref: ${{") && b.contains("outputs.tag"),
        "build must check out the exact resolved tag, not the moving default branch"
    );
    assert!(
        b.contains("cargo build --release --locked"),
        "build must use `cargo build --release --locked`"
    );
    assert!(
        b.contains("upload-artifact@v4"),
        "build must upload per-target workflow artifacts"
    );
    assert!(
        b.contains("permissions:")
            && b.contains("contents: read")
            && !b.contains("contents: write"),
        "build must have read-only contents permission"
    );
}

/// The matrix is exactly the five documented target triples, each appearing
/// once, with `fail-fast: false`.
#[test]
fn build_matrix_is_exact_five_targets_fail_fast_false() {
    let wf = workflow_text();
    let b = job_block(&wf, "build").expect("missing job `build`");
    assert!(
        b.contains("strategy:") && b.contains("matrix:"),
        "build must declare a strategy matrix"
    );
    assert!(
        b.contains("fail-fast: false"),
        "matrix must keep `fail-fast: false` so all platform results remain visible"
    );
    for t in TARGET_TRIPLES {
        assert_eq!(
            wf.matches(t).count(),
            1,
            "target triple `{t}` must appear exactly once in the workflow"
        );
    }
}

/// publish needs both resolve-release and build, guards on a non-empty tag,
/// preflights the release with `gh release view` (write-scoped token) BEFORE
/// artifact download, aggregates downloads into one dist/ directory BEFORE
/// ten-file validation, verifies every archive's adjacent .sha256 digest,
/// installs canonical changelog notes, uploads with clobber semantics, and
/// undrafts only after success.
#[test]
fn publish_job_aggregates_downloads_validates_and_undrafts_last() {
    let p = job_block(&workflow_text(), "publish").expect("missing job `publish`");
    assert!(
        p.contains("needs:") && p.contains("resolve-release") && p.contains("build"),
        "publish must declare `needs: [resolve-release, build]`"
    );
    assert!(
        p.contains("needs.resolve-release.outputs.tag") && p.contains("!= ''"),
        "publish must guard on a non-empty resolved tag"
    );
    assert!(
        p.contains("always()")
            && p.contains("needs.resolve-release.result == 'success'")
            && p.contains("needs.build.result == 'success'"),
        "publish must override manual-dispatch skip propagation while still requiring both direct dependencies to succeed"
    );
    assert!(
        p.contains("download-artifact@v4"),
        "publish must aggregate artifacts with actions/download-artifact@v4"
    );
    assert!(
        p.contains("path: dist"),
        "downloaded artifacts must merge into one dist/ directory"
    );
    let download = line_of(&p, "download-artifact@v4").expect("download step missing");
    assert!(
        p.contains("gh release view"),
        "publish must preflight the release with `gh release view` (write-scoped GITHUB_TOKEN) before artifact download"
    );
    let preflight = line_of(&p, "gh release view").expect("release preflight missing");
    assert!(
        preflight < download,
        "the `gh release view` preflight must run before artifact download/upload"
    );
    assert!(
        p.contains("find dist"),
        "ten-file validation must enumerate the dist/ directory with `find dist`"
    );
    assert!(
        p.contains("wc -l"),
        "ten-file validation must count files with `wc -l`"
    );
    let count_line = p
        .lines()
        .position(|l| l.contains("-eq") && l.contains("expected_total"))
        .unwrap_or_else(|| {
            panic!("ten-file validation must compare the file count against the matrix-derived expected total (e.g. `-eq \"$expected_total\"`)")
        });
    assert!(
        download < count_line,
        "artifact download must precede the ten-file validation step"
    );
    assert!(
        p.contains("adjacent"),
        "publish must require every archive to have its adjacent .sha256 checksum before upload"
    );
    assert!(
        p.contains("sha256sum -c"),
        "publish must verify every archive's digest with `sha256sum -c` before upload"
    );
    assert!(
        p.contains("--clobber"),
        "uploads must use clobber semantics so recovery reruns converge"
    );
    assert!(
        p.contains("--draft=false"),
        "the release must be undrafted only after all uploads succeed"
    );
    let clobber = line_of(&p, "--clobber").expect("clobber upload missing");
    let undraft = line_of(&p, "--draft=false").expect("undraft step missing");
    assert!(clobber < undraft, "clobber upload must precede undrafting");
    assert!(
        p.contains("extract --version"),
        "canonical notes must be extracted from the finalized changelog"
    );
    assert!(
        p.contains("gh release edit") && p.contains("--notes-file"),
        "the draft body must be replaced with the changelog-derived notes"
    );
    assert!(
        p.contains("permissions:") && p.contains("contents: write"),
        "only publish may request contents: write"
    );
}

/// Archive naming comes from the resolved tag, never `GITHUB_REF_NAME`;
/// no crates.io publication; only the repository GITHUB_TOKEN is referenced.
#[test]
fn no_ref_name_naming_or_crates_io_in_workflow() {
    let wf = workflow_text();
    assert!(
        !wf.contains("GITHUB_REF_NAME"),
        "archive names must derive from the resolved tag output, not GITHUB_REF_NAME"
    );
    assert!(
        !wf.contains("crates.io") && !wf.contains("cargo publish"),
        "the workflow must not publish to crates.io"
    );
    for (i, line) in wf.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        assert!(
            !line.contains("0.1.0"),
            "the workflow must not hardcode the package version (version lives in Cargo.toml); line {}: {line}",
            i + 1
        );
    }
    for line in wf.lines() {
        let t = line.trim();
        if t.contains("secrets.") {
            assert!(
                t.contains("secrets.GITHUB_TOKEN"),
                "only the repository GITHUB_TOKEN may be referenced; found: {t}"
            );
        }
    }
}

/// Every `uses:` dependency is pinned to a maintained major tag `@v<integer>`
/// — never a floating branch, hash, or full version.
#[test]
fn all_uses_are_maintained_major_tags() {
    let wf = workflow_text();
    let refs = uses_refs(&wf);
    assert!(
        !refs.is_empty(),
        "workflow must reference at least one pinned action"
    );
    for (repo, r) in &refs {
        assert!(
            repo.contains('/'),
            "uses `{repo}@{r}` must be owner/repo form"
        );
        assert!(
            r.starts_with('v') && r.len() > 1 && r[1..].chars().all(|c| c.is_ascii_digit()),
            "action `{repo}@{r}` must be pinned to a maintained major tag @v<integer>, not a floating ref"
        );
    }
}

// ---------------------------------------------------------------------------
// Release Please configuration contract (FR-2)
// ---------------------------------------------------------------------------

/// release-please-config.json disables Release Please's own changelog
/// generation, creates draft releases, and forces tag creation so binary
/// jobs can check out the tag before publication.
#[test]
fn release_please_config_skip_changelog_draft_forced_tags() {
    let cfg = config_text();
    assert!(
        json_key_value(&cfg, "skip-changelog", "true"),
        "release-please-config.json must set skip-changelog: true (the finalizer is the only changelog writer)"
    );
    assert!(
        json_key_value(&cfg, "draft", "true"),
        "release-please-config.json must create draft releases (draft: true)"
    );
    assert!(
        json_key_value(&cfg, "force-tag-creation", "true"),
        "release-please-config.json must force tag creation (force-tag-creation: true)"
    );
    assert!(
        json_key_value(&cfg, "release-type", "rust"),
        "release-please-config.json must use the Rust release type"
    );
    assert!(
        json_root_package_object(&cfg),
        "release-please-config.json must define `packages` containing a root `\".\"` package object"
    );
    assert!(
        json_key_value(&cfg, "include-v-in-tag", "true"),
        "release-please-config.json must create v-prefixed tags (include-v-in-tag: true)"
    );
    assert!(
        json_key_value(&cfg, "include-component-in-tag", "false"),
        "release-please-config.json must keep component names out of tags (include-component-in-tag: false)"
    );
}

/// .release-please-manifest.json initializes the root package at 0.1.0.
#[test]
fn release_please_manifest_initializes_llmu_at_0_1_0() {
    let m = manifest_text();
    assert!(
        json_key_value(&m, ".", "0.1.0"),
        ".release-please-manifest.json must initialize the root package at 0.1.0"
    );
}

// ---------------------------------------------------------------------------
// documentation contract (FR-5)
// ---------------------------------------------------------------------------

/// README documents the release-PR lifecycle instead of tag pushes:
/// conventional-commit bumps, Unreleased contributions, draft publication
/// safety, manual recovery, canonical changelog, and an explicit statement
/// that releases are GitHub Release binaries, not crates.io publications.
#[test]
fn readme_documents_release_pr_flow() {
    let r = readme_text();
    let plain = r.replace('`', "");
    assert!(
        !plain.contains("v* tag"),
        "README must not claim releases are produced on pushed `v*` tags"
    );
    assert!(
        r.contains("Release Please") || r.contains("release PR"),
        "README must document the release-PR lifecycle"
    );
    assert!(
        r.contains("Conventional Commit") || r.contains("conventional commit"),
        "README must document Conventional Commit bump rules (fix/feat/breaking)"
    );
    assert!(
        r.contains("Unreleased"),
        "README must direct contributors to add notes under the Unreleased section"
    );
    assert!(
        r.contains("CHANGELOG") && r.contains("canonical"),
        "README must state that CHANGELOG.md is canonical for release notes"
    );
    assert!(
        r.contains("draft"),
        "README must document draft publication safety"
    );
    assert!(
        r.contains("workflow_dispatch") || r.contains("manual"),
        "README must document manual tag recovery"
    );
    assert!(
        r.contains("musl") && r.contains("sha256"),
        "README must keep the five-target and checksum promises aligned with the matrix"
    );
    assert!(
        r.contains("GitHub Release")
            && r.contains("not published to crates.io"),
        "README must explicitly state that llmu releases are GitHub Release binaries and are not published to crates.io"
    );
}

// ---------------------------------------------------------------------------
// Task 5 review-blocker contracts
// ---------------------------------------------------------------------------

/// `finalize` reads the root `[package]` version even when the `version =`
/// line carries a trailing inline TOML comment (`version = "0.2.0" # note`).
#[test]
fn finalize_accepts_trailing_inline_toml_comment_on_version() {
    let tmp = TempDir::new();
    let changelog = write_fixture(&tmp, "CHANGELOG.md", &changelog_fixture());
    let cargo = write_fixture(
        &tmp,
        "Cargo.toml",
        "[package]\nname = \"llmu\"\nversion = \"0.2.0\"  # bumped for the 0.2.0 release\n",
    );

    let out = run_finalizer(
        &[
            "finalize",
            "--changelog",
            changelog.to_str().expect("utf8 path"),
            "--cargo",
            cargo.to_str().expect("utf8 path"),
            "--date",
            "2026-08-11",
        ],
        &repo_root(),
    );
    assert!(
        out.status.success(),
        "finalize must accept a trailing inline TOML comment after the package version; stderr: {}",
        stderr_of(&out)
    );
    let new = fs::read_to_string(&changelog).expect("read finalized changelog");
    assert_eq!(
        count(&new, "## [0.2.0] - 2026-08-11"),
        1,
        "the version with the trailing comment must still finalize into the dated heading\n{new}"
    );
}

/// A standard MIT LICENSE exists (copyright 2026 xoai, permission clause,
/// warranty disclaimer), and the build packaging steps place it flat beside
/// the binary in every tar/zip archive.
#[test]
fn license_exists_with_mit_text_and_is_packaged_flat_beside_binary() {
    let lic = read_named(&repo_root().join("LICENSE"), "LICENSE");
    assert!(
        lic.contains("MIT License"),
        "LICENSE must carry the MIT license name"
    );
    assert!(
        lic.contains("Copyright (c) 2026 xoai"),
        "LICENSE must be copyrighted 2026 xoai"
    );
    assert!(
        lic.contains("Permission is hereby granted, free of charge"),
        "LICENSE must carry the standard MIT permission clause"
    );
    assert!(
        lic.contains("WITHOUT WARRANTY OF ANY KIND"),
        "LICENSE must carry the standard MIT warranty disclaimer"
    );

    let b = job_block(&workflow_text(), "build").expect("missing job `build`");
    let cp_line = line_of(&b, "cp LICENSE")
        .and_then(|i| b.lines().nth(i))
        .expect("build must copy LICENSE into the release directory before packaging");
    let tar_line = line_of(&b, "tar -C")
        .and_then(|i| b.lines().nth(i))
        .expect("tar packaging step missing");
    assert!(
        tar_line.contains("LICENSE"),
        "tar must place LICENSE flat beside the binary in the archive, got: {tar_line}"
    );
    let zip_line = line_of(&b, "Compress-Archive")
        .and_then(|i| b.lines().nth(i))
        .expect("pwsh packaging step missing");
    assert!(
        zip_line.contains("LICENSE"),
        "zip must place LICENSE flat beside the binary in the archive, got: {zip_line}"
    );
    assert!(
        cp_line.contains("LICENSE"),
        "packaging must copy the repository LICENSE first: {cp_line}"
    );
}

/// `.gitignore` ignores Python bytecode (`__pycache__/` and `*.pyc`).
#[test]
fn gitignore_ignores_python_bytecode() {
    let gi = read_named(&repo_root().join(".gitignore"), ".gitignore");
    assert!(
        gi.lines().any(|l| l.trim() == "__pycache__/"),
        ".gitignore must ignore `__pycache__/`"
    );
    assert!(
        gi.lines().any(|l| l.trim() == "*.pyc"),
        ".gitignore must ignore `*.pyc`"
    );
}

/// release-please-config.json creates release PRs as drafts so nothing can
/// merge before the finalizer commit lands on the PR branch.
#[test]
fn release_please_config_drafts_pull_requests() {
    let cfg = config_text();
    assert!(
        json_key_value(&cfg, "draft-pull-request", "true"),
        "release-please-config.json must set draft-pull-request: true (release PRs start as drafts)"
    );
}

/// The workflow sets a least-privilege top-level `permissions: contents: read`
/// default (never write) and a per-ref concurrency group that never cancels a
/// run already publishing.
#[test]
fn workflow_top_level_readonly_permissions_and_non_cancelling_concurrency() {
    let wf = workflow_text();
    let perms = top_level_block(&wf, "permissions")
        .unwrap_or_else(|| panic!("top-level `permissions:` block missing"));
    assert!(
        perms.contains("contents: read"),
        "top-level permissions must default to `contents: read`\n{perms}"
    );
    assert!(
        !perms.contains("contents: write"),
        "top-level permissions must never grant write\n{perms}"
    );
    let conc = top_level_block(&wf, "concurrency")
        .unwrap_or_else(|| panic!("top-level `concurrency:` block missing"));
    assert!(
        conc.contains("group: release-${{ github.ref }}"),
        "concurrency must be grouped per ref (`group: release-${{ github.ref }}`)\n{conc}"
    );
    assert!(
        conc.contains("cancel-in-progress: false"),
        "concurrency must never cancel a run already in progress\n{conc}"
    );
}

/// The release-please job drafts any existing open `autorelease: pending`
/// PR before the action runs (so nothing can merge before the finalizer
/// lands), and marks the action-returned PR number ready only after the
/// finalizer commit/push step.
#[test]
fn release_please_job_drafts_pending_pr_before_action_and_readies_after_commit() {
    let rp = job_block(&workflow_text(), "release-please").expect("missing job `release-please`");
    assert!(
        rp.contains("autorelease: pending"),
        "the pre-action draft step must target the default `autorelease: pending` lifecycle label"
    );
    assert!(
        rp.contains("--undo"),
        "a ready-but-pending PR must be converted back to draft with `gh pr ready <n> --undo`"
    );
    let draft_scan = line_of(&rp, "gh pr list").expect("pre-action draft step missing");
    let action = line_of(&rp, "googleapis/release-please-action@v4")
        .expect("release-please action step missing");
    assert!(
        draft_scan < action,
        "existing pending PRs must be drafted BEFORE the release-please action runs"
    );
    assert!(
        rp.contains("fromJSON(steps.release-please.outputs.prs)[0].number"),
        "the PR number must be extracted from the action's `prs` JSON (e.g. `fromJSON(...)[0].number`) — the raw `pr` output is a JSON object, not a number"
    );
    for line in rp.lines() {
        assert!(
            !(line.contains("outputs.pr") && !line.contains("outputs.prs")),
            "the raw `pr` output (a JSON object) must never be passed to gh; found: {line}"
        );
    }
    let ready_step = line_of(&rp, "Mark release PR ready").expect("ready step missing");
    assert!(
        rp.lines()
            .nth(ready_step + 1)
            .map(|l| l.trim_start().starts_with("if:"))
            .unwrap_or(false),
        "the ready step must be gated like the other PR work"
    );
    let commit_step =
        line_of(&rp, "Commit and push finalized CHANGELOG").expect("finalizer commit step missing");
    let finalize_step = line_of(&rp, "finalize_changelog.py").expect("finalizer step missing");
    assert!(
        finalize_step < commit_step,
        "the finalizer must run before its commit"
    );
    assert!(
        action < commit_step && commit_step < ready_step,
        "the PR must be marked ready only AFTER the finalizer commit/push step"
    );
}

/// The pre-action draft scan must capture an already-open `autorelease:
/// pending` PR's number AND head branch into step outputs, and every
/// post-action PR step must fall back to those captured values when
/// `prs_created` is false (e.g. a chore-only push while the release PR is
/// already open): the branch is still checked out and finalized and the same
/// PR is restored ready — never left indefinitely draft with stale notes.
#[test]
fn release_please_job_captures_pending_pr_and_falls_back_when_prs_created_false() {
    let rp = job_block(&workflow_text(), "release-please").expect("missing job `release-please`");
    let lines: Vec<&str> = rp.lines().collect();
    let action = line_of(&rp, "googleapis/release-please-action@v4")
        .expect("release-please action step missing");

    // Capture half: number + head branch, persisted before the action runs.
    let scan = line_of(&rp, "gh pr list").expect("pre-action pending-PR scan missing");
    assert!(
        scan < action,
        "the pending-PR scan must run BEFORE the release-please action so its captured values can serve as the fallback"
    );
    let scan_line = lines[scan];
    assert!(
        scan_line.contains("headRefName"),
        "the pending-PR scan must capture the head branch together with the number (`gh pr list --json number,headRefName`); got: {scan_line}"
    );
    assert!(
        scan_line.contains("number"),
        "the pending-PR scan must capture the PR number; got: {scan_line}"
    );
    let pre_action = lines[scan..action].join("\n");
    assert!(
        pre_action.contains("GITHUB_OUTPUT"),
        "the captured PR number and head branch must be persisted to step outputs (e.g. `echo \"number=..\" >> \"$GITHUB_OUTPUT\"`) before the action"
    );
    assert!(
        rp.contains("GH_REPO: ${{ github.repository }}"),
        "the pre-checkout `gh pr list` step must bind GH_REPO explicitly because no local Git repository exists yet"
    );

    // Fallback half: with `prs_created` false, the captured values drive the
    // checkout (finalization) and the ready step.
    let checkout =
        line_of(&rp, "- name: Checkout release PR head branch").expect("checkout step missing");
    let checkout_if = lines[checkout + 1];
    assert!(
        checkout_if.trim_start().starts_with("if:") && checkout_if.contains("outputs.number"),
        "the checkout step must also run when the captured pending PR exists, not only when `prs_created` is true; got: {checkout_if}"
    );
    let checkout_ref = lines[line_of(&rp, "ref: ${{").expect("checkout ref missing")];
    assert!(
        checkout_ref.contains("outputs.branch"),
        "the checkout `ref:` must fall back to the captured head branch when `prs_created` is false; got: {checkout_ref}"
    );
    let ready_env = lines[line_of(&rp, "PR_NUMBER:").expect("ready-step PR_NUMBER env missing")];
    assert!(
        ready_env.contains("outputs.number"),
        "the ready step must fall back to the captured PR number when `prs_created` is false; got: {ready_env}"
    );
}

/// The release-please job explicitly retains `issues: write` — the default
/// lifecycle labels (`autorelease: pending` / `autorelease: ready`) are
/// managed through the Issues API, not the Pulls API — even though the
/// workflow-wide default is `contents: read`.
#[test]
fn release_please_job_retains_issues_write_for_lifecycle_labels() {
    let rp = job_block(&workflow_text(), "release-please").expect("missing job `release-please`");
    assert!(
        rp.contains("issues: write"),
        "release-please must keep `issues: write`: lifecycle labels are set through the Issues API"
    );
    assert!(
        rp.contains("Issues API") || rp.contains("issues API"),
        "the workflow must document that issues: write is retained for Issues-API lifecycle labels"
    );
}

/// publish installs the canonical changelog notes BEFORE undrafting the
/// release, so the published release never carries the placeholder body.
#[test]
fn publish_notes_replacement_precedes_undraft() {
    let p = job_block(&workflow_text(), "publish").expect("missing job `publish`");
    let notes = line_of(&p, "--notes-file").expect("notes replacement step missing");
    let undraft = line_of(&p, "--draft=false").expect("undraft step missing");
    assert!(
        notes < undraft,
        "the canonical notes replacement must precede `--draft=false`"
    );
}

/// publish's expected asset count is a static env literal that must stay
/// coupled to the matrix length: one archive + one adjacent checksum per
/// matrix leg. The build job must NOT emit a matrix-size output —
/// same-key outputs from matrix legs are scalars, not arrays, so
/// `fromJSON(needs.build.outputs.<name>)[0]` would fail at runtime.
#[test]
fn publish_expected_assets_match_matrix_length() {
    let expected = TARGET_TRIPLES.len();
    let p = job_block(&workflow_text(), "publish").expect("missing job `publish`");
    assert!(
        p.contains(&format!("EXPECTED_TARGETS: {expected}")),
        "publish must set `EXPECTED_TARGETS: {expected}` (one archive + one checksum per matrix leg)"
    );
    assert!(
        !p.contains("needs.build.outputs") && !p.contains("fromJSON(needs.build.outputs"),
        "publish must not consume build matrix outputs: same-key matrix outputs do not merge into arrays"
    );
    assert!(
        !p.contains("-eq 10") && !p.contains("== 10"),
        "the shell comparison must use the EXPECTED_TARGETS-derived total, not a hardcoded 10"
    );
    let b = job_block(&workflow_text(), "build").expect("missing job `build`");
    assert!(
        !b.contains("job-count") && !b.contains("expected_targets"),
        "build must not emit a matrix-size output (same-key matrix outputs stay scalars)"
    );
}

/// Job scanning must ignore comment-only YAML lines: comments are structural
/// noise that must never terminate or corrupt job/step block detection.
#[test]
fn job_scanning_ignores_comment_only_yaml_lines() {
    let wf = workflow_text();
    let with_comments = wf.replacen(
        "jobs:",
        "jobs:\n# comment-only line between jobs\n# another one: with a colon\n",
        1,
    );
    let ids = job_ids(&with_comments);
    assert_eq!(
        ids.len(),
        EXPECTED_JOBS.len(),
        "comment-only YAML lines must not truncate job detection; found: {ids:?}"
    );
    for id in EXPECTED_JOBS {
        assert!(ids.iter().any(|x| x == id), "missing job `{id}` in {ids:?}");
    }
    let rp = job_block(&with_comments, "release-please")
        .expect("comment-only lines must not break job_block extraction");
    assert!(
        rp.contains("runs-on:"),
        "job body must survive comment skipping"
    );
}

/// Every `run:` block binds GitHub contexts (github.*, needs.*, inputs.*,
/// steps.*) through the step `env:` map instead of interpolating expressions
/// directly into shell text; only `matrix.*` values remain inline where no
/// practical alternative exists (e.g. `rustup target add`).
#[test]
fn shell_run_blocks_bind_contexts_through_env() {
    let wf = workflow_text();
    let blocks = run_blocks(&wf);
    assert!(
        blocks.len() >= 4,
        "expected several run blocks, found {}",
        blocks.len()
    );
    for block in &blocks {
        let mut from = 0;
        while let Some(rel) = block[from..].find("${{") {
            let idx = from + rel;
            let end = block[idx..]
                .find("}}")
                .map(|e| idx + e + 2)
                .unwrap_or(block.len());
            let expr = &block[idx + 3..end - 2].trim();
            assert!(
                !(expr.starts_with("github.")
                    || expr.starts_with("needs.")
                    || expr.starts_with("inputs.")
                    || expr.starts_with("steps.")),
                "run block must bind `${{ {expr} }}` through step env instead of inline shell interpolation:\n{block}"
            );
            from = end;
        }
    }
}
