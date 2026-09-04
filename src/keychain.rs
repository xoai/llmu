//! Read-only macOS Keychain credential reads (FR-2, FR-4.1).
//!
//! Claude Code on macOS stores its `claudeAiOauth` blob as a
//! generic-password item in the login keychain rather than as the
//! plaintext `.credentials.json` that the Linux/WSL layout uses. Without
//! this module llmu finds no Claude Code credentials on a Mac at all and
//! silently falls back to whatever plaintext access token it can
//! discover (typically OpenCode's), which no process refreshes once its
//! owner stops running.
//!
//! Access goes through `/usr/bin/security`, inheriting exactly the
//! keychain ACL the invoking terminal already holds. Two consequences
//! worth knowing:
//!
//! - `security` resolves the login keychain under `$HOME`, so a test or
//!   sandbox that overrides `HOME` is automatically isolated from the
//!   developer's real keychain. The integration sandboxes rely on this.
//! - A locked keychain can put up a GUI unlock prompt, which would
//!   otherwise block a `security` child forever. Every invocation is
//!   therefore bounded by [`READ_TIMEOUT`] and the child is killed on
//!   expiry — a usage monitor must never hang a CLI run or a TUI tick.
//!
//! This module is **strictly read-only**: llmu never adds, updates,
//! deletes, or unlocks a keychain item. That is a safety property, not
//! just an implementation detail — OAuth refresh rotates the refresh
//! token, so a writer that rotated Claude Code's token without
//! atomically updating the keychain would invalidate the stored refresh
//! token and log the user out of Claude Code. Rotation therefore stays
//! owned by the client that created the item, and llmu consumes the
//! access token as-is.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The generic-password service Claude Code stores its OAuth blob under
/// for a default installation.
///
/// A non-default `CLAUDE_CONFIG_DIR` makes Claude Code append an
/// installation-scoped suffix (`Claude Code-credentials-<8 hex>`). That
/// suffix is not derivable from anything llmu can see, and a keychain
/// commonly holds many such items, so guessing one risks reading a
/// different installation's credentials. Those setups name their item
/// explicitly via `[claude] keychain_service` instead.
pub const CLAUDE_CODE_SERVICE: &str = "Claude Code-credentials";

/// The `security` binary is addressed by absolute path so a shadowed
/// `security` earlier in `PATH` can never be executed instead.
const SECURITY_BIN: &str = "/usr/bin/security";

/// Upper bound on a single `security` invocation. Generous for the
/// normal unlocked read (milliseconds) and short enough that a keychain
/// unlock prompt degrades to "unavailable" instead of hanging llmu.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll cadence while waiting for the bounded child.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long one [`has_item`] answer is reused.
///
/// Long enough to collapse the several probes a single fetch round makes
/// (`configured`, `quotas_impl`, and discovery all ask), short enough
/// that no answer outlives one refresh cycle of `llmu watch`. Caching an
/// answer for the life of the process was the wrong trade for a
/// long-running dashboard: a keychain that happened to be locked at
/// launch, a `security` call that hit [`READ_TIMEOUT`], or a Claude Code
/// login that came later would all pin "no keychain item" forever, and
/// only restarting llmu could undo it.
const PROBE_TTL: Duration = Duration::from_secs(20);

/// Whether this build can read a keychain at all. Keychain support is
/// macOS-only; every other target keeps the plaintext-file behavior.
pub const fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// The keychain account Claude Code files its item under: the OS user.
///
/// Matching it matters because a keychain can hold several items sharing
/// one service name; a service-only lookup would return an arbitrary
/// one. `None` (no `-a` selector) when the username is unknown.
fn account() -> Option<String> {
    std::env::var("USER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Run `security` with a hard deadline, killing the child if it exceeds
/// it (a locked keychain can otherwise block on a GUI prompt forever).
///
/// stdout/stderr are piped but only read after exit. Safe here because
/// the payloads involved are a few kilobytes at most — far below the
/// pipe buffer — so the child cannot block on a full pipe.
fn security(args: &[&str]) -> Result<std::process::Output> {
    let mut child = Command::new(SECURITY_BIN)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {SECURITY_BIN}"))?;

    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "{SECURITY_BIN} did not respond within {}s — the login keychain is \
                         probably locked and waiting on an unlock prompt",
                        READ_TIMEOUT.as_secs()
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).context("waiting for the keychain read to finish");
            }
        }
    }
    child
        .wait_with_output()
        .context("collecting the keychain read output")
}

/// Argument vector for one generic-password lookup. `-w` asks for the
/// secret; without it only attributes are consulted, which never raises
/// an ACL prompt.
fn lookup_args<'a>(service: &'a str, acct: Option<&'a str>, secret: bool) -> Vec<&'a str> {
    let mut args = vec!["find-generic-password", "-s", service];
    if let Some(a) = acct {
        args.push("-a");
        args.push(a);
    }
    if secret {
        args.push("-w");
    }
    args
}

/// Look the item up with the account selector first, then without it.
///
/// The retry keeps llmu working where the item was filed under a
/// different account than `$USER` (a migrated or hand-created item)
/// while still preferring the exact match Claude Code writes.
fn lookup(service: &str, secret: bool) -> Result<std::process::Output> {
    let acct = account();
    if let Some(a) = acct.as_deref() {
        let out = security(&lookup_args(service, Some(a), secret))?;
        if out.status.success() {
            return Ok(out);
        }
    }
    security(&lookup_args(service, None, secret))
}

/// Short-lived [`has_item`] results, keyed by service name. Several
/// probes run per fetch round (`configured()`, `quotas_impl`, and
/// discovery each ask), so one `security` spawn covers them all.
fn probe_cache() -> &'static Mutex<HashMap<String, (bool, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (bool, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether a memoized answer recorded at `at` may still be reused.
/// Split out so the expiry rule is testable without waiting on a clock.
fn probe_is_fresh(at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(at) < PROBE_TTL
}

/// Cheap read-only existence probe: an attribute lookup with no `-w`, so
/// the item's secret is never requested and no ACL prompt can appear.
///
/// Memoized for [`PROBE_TTL`] rather than for the process: a dashboard
/// runs for hours, and an answer that can never be revisited turns any
/// momentary failure — a locked keychain, a timed-out `security`, a
/// login that has not happened yet — into a permanent one that only a
/// restart clears.
pub fn has_item(service: &str) -> bool {
    if !supported() {
        return false;
    }
    let now = Instant::now();
    if let Ok(cache) = probe_cache().lock() {
        if let Some((hit, at)) = cache.get(service) {
            if probe_is_fresh(*at, now) {
                return *hit;
            }
        }
    }
    let present = lookup(service, false)
        .map(|o| o.status.success())
        .unwrap_or(false);
    if let Ok(mut cache) = probe_cache().lock() {
        cache.insert(service.to_string(), (present, Instant::now()));
    }
    present
}

/// Read `service`'s generic-password payload and parse it as JSON.
///
/// Diagnostics stay secret-free: `security`'s stderr carries only status
/// text, and a parse failure names the service rather than echoing the
/// payload.
pub fn read_json(service: &str) -> Result<Value> {
    if !supported() {
        bail!("keychain credential storage is only supported on macOS");
    }
    let out = lookup(service, true)?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "keychain service {service:?} could not be read ({}) — unlock the login keychain or log in again with Claude Code",
            if detail.is_empty() {
                "no such item".to_string()
            } else {
                detail
            }
        );
    }
    let raw = String::from_utf8(out.stdout)
        .with_context(|| format!("keychain service {service:?} payload is not UTF-8"))?;
    serde_json::from_str(raw.trim())
        .with_context(|| format!("parsing keychain service {service:?} payload as JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_matches_target_os() {
        assert_eq!(supported(), cfg!(target_os = "macos"));
    }

    #[test]
    fn claude_code_service_is_the_upstream_item_name() {
        // Claude Code's keychain item name; changing it silently breaks
        // macOS credential discovery.
        assert_eq!(CLAUDE_CODE_SERVICE, "Claude Code-credentials");
    }

    #[test]
    fn missing_service_is_an_error_not_a_panic() {
        let e = read_json("llmu-nonexistent-service-probe")
            .expect_err("a service that cannot exist must not resolve");
        let msg = e.to_string();
        if supported() {
            assert!(
                msg.contains("could not be read"),
                "macOS reports an unreadable item: {msg}"
            );
        } else {
            assert!(
                msg.contains("only supported on macOS"),
                "other targets report unsupported: {msg}"
            );
        }
    }

    #[test]
    fn absent_item_probes_false_and_memoizes() {
        let svc = "llmu-nonexistent-service-probe-2";
        assert!(!has_item(svc));
        // Second call is served from the memo; still false.
        assert!(!has_item(svc));
    }

    /// The memo must expire. A permanently cached "no such item" is how a
    /// long-running `llmu watch` loses Claude quotas for good after one
    /// locked-keychain or timed-out probe.
    #[test]
    fn probe_memo_expires_so_a_failed_probe_is_never_permanent() {
        assert!(
            PROBE_TTL >= Duration::from_secs(1) && PROBE_TTL <= Duration::from_secs(60),
            "long enough to collapse one fetch round, short enough to self-heal"
        );
        let now = Instant::now();
        assert!(probe_is_fresh(now, now), "a just-recorded answer is reused");
        assert!(
            probe_is_fresh(now, now + PROBE_TTL - Duration::from_millis(1)),
            "an answer inside the window is reused"
        );
        assert!(
            !probe_is_fresh(now, now + PROBE_TTL),
            "an answer at the horizon is re-probed"
        );
        assert!(
            !probe_is_fresh(now, now + PROBE_TTL * 100),
            "no answer survives the process"
        );
    }

    #[test]
    fn security_binary_is_absolute() {
        assert!(
            SECURITY_BIN.starts_with('/'),
            "an absolute path prevents PATH shadowing"
        );
    }

    /// The account selector is what disambiguates a keychain holding
    /// several items under one service name.
    #[test]
    fn lookup_args_carry_service_account_and_secret_flags() {
        assert_eq!(
            lookup_args("Svc", Some("me"), true),
            vec!["find-generic-password", "-s", "Svc", "-a", "me", "-w"]
        );
        assert_eq!(
            lookup_args("Svc", Some("me"), false),
            vec!["find-generic-password", "-s", "Svc", "-a", "me"],
            "the probe must not request the secret (no ACL prompt)"
        );
        assert_eq!(
            lookup_args("Svc", None, true),
            vec!["find-generic-password", "-s", "Svc", "-w"],
            "an unknown account degrades to a service-only lookup"
        );
    }

    #[test]
    fn read_timeout_is_bounded_and_nonzero() {
        assert!(
            READ_TIMEOUT >= Duration::from_secs(1) && READ_TIMEOUT <= Duration::from_secs(30),
            "the bound must be long enough for a normal read and short enough to never hang llmu"
        );
        assert!(POLL_INTERVAL < READ_TIMEOUT);
    }

    /// A bounded run must return promptly for a fast child rather than
    /// waiting out the deadline.
    #[test]
    fn bounded_security_run_returns_before_the_deadline() {
        if !supported() {
            return;
        }
        let start = Instant::now();
        // `-h` exits immediately regardless of keychain state.
        let _ = security(&["help"]);
        assert!(
            start.elapsed() < READ_TIMEOUT,
            "a fast child must not wait out the deadline"
        );
    }
}
