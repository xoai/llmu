//! Rotation-safe credential file transactions (AD-2, FR-4).
//!
//! This module owns only safe local mutation of a supported plaintext
//! credential file: secure read, same-directory temporary write, Unix
//! `0600`, pre-replace refresh-token compare-and-swap, rename, and
//! post-write verification. Provider OAuth schemas never live here —
//! callers pass parsed JSON plus provider-owned extractor/validation
//! closures and a provider-owned merge closure (FR-4.2).
//!
//! Concurrency uses an llmu-only sibling lock
//! (`<credential-filename>.llmu-refresh.lock`) acquired with
//! `create_new`; llmu never creates, deletes, or claims compatibility
//! with upstream CLI lock files (FR-4.7). Cross-tool safety therefore
//! rests on the refresh-token compare-and-swap immediately before
//! rename; the residual non-atomic check/rename race against writers
//! that ignore the llmu lock is acknowledged rather than hidden (AD-2).

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Sibling lock filename suffix (FR-4.7): the lock for a credential
/// file `creds.json` is `creds.json.llmu-refresh.lock`. The suffix is
/// llmu-owned, so no provider lock is ever inspected or deleted.
pub const LOCK_SUFFIX: &str = ".llmu-refresh.lock";

/// Default acquisition cadence (FR-4.7): retry every 100ms for at most
/// five seconds. Tests inject shorter values.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(100);
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// A lock whose modification time is older than this is stale: remove
/// it and retry `create_new` (FR-4.7). Refresh HTTP is capped at 30
/// seconds, so 60 seconds cannot hold a live llmu refresh.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(60);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Per-attempt nonce source for lock metadata and temp names.
static NONCE: AtomicU64 = AtomicU64::new(0);

/// Test-injectable lock timing. Production defaults are FR-4.7's 100ms
/// retry and five-second timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockTiming {
    pub retry_delay: Duration,
    pub timeout: Duration,
}

impl Default for LockTiming {
    fn default() -> Self {
        LockTiming {
            retry_delay: LOCK_RETRY_DELAY,
            timeout: LOCK_TIMEOUT,
        }
    }
}

/// Provider-owned knowledge of the payload shape (AD-2). The module
/// stays schema-agnostic: `refresh_token` locates the CAS field inside
/// a parsed root object; `validate` runs on the merged result before
/// the temp write and again on the post-rename re-read (FR-4.5, FR-4.6).
pub struct CredentialSchema<'a> {
    /// Extract the stored refresh token from a parsed root object.
    pub refresh_token: &'a dyn Fn(&Value) -> Option<&str>,
    /// Provider-owned validation of all required response fields.
    pub validate: &'a dyn Fn(&Value) -> Result<()>,
}

/// What `replace_with_cas` did.
#[derive(Debug)]
pub enum ReplaceOutcome {
    /// The merged value was validated, written, and re-read successfully.
    Replaced { value: Value },
    /// Under the lock the stored refresh token already differed from the
    /// request token: another llmu process refreshed first. `value` is
    /// the freshly re-read root so the caller can adopt the new token
    /// and skip its own duplicate refresh (FR-4.3, AS-4).
    ChangedByOther { value: Value },
}

/// RAII guard for the llmu refresh lock. The lock file is deleted on
/// release only while it still carries exactly this attempt's metadata,
/// so a lock that was stale-recovered and re-created by another process
/// is never deleted (FR-4.7).
#[derive(Debug)]
struct RefreshLock {
    path: PathBuf,
    /// `pid\nepoch_ms\nnonce\n` — the exact bytes this attempt wrote.
    contents: String,
    armed: bool,
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        // Nonce-safe release (FR-4.7): delete only while the lock file
        // still carries this attempt's pid/epoch/nonce. A foreign or
        // re-created lock never matches and is left alone.
        if fs::read_to_string(&self.path).as_deref().ok() == Some(self.contents.as_str()) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Sibling lock path for a credential file (FR-4.7).
fn lock_path(credential: &Path) -> Result<PathBuf> {
    let name = credential.file_name().ok_or_else(|| {
        anyhow::anyhow!("credential path {} has no file name", credential.display())
    })?;
    Ok(credential.with_file_name(format!("{}{LOCK_SUFFIX}", name.to_string_lossy())))
}

/// A lock whose modification time is more than `LOCK_STALE_AFTER` in the
/// past is stale (FR-4.7): refresh HTTP is capped at 30 seconds, so a
/// live llmu refresh can never hold the lock that long. Unreadable
/// metadata and future mtimes are treated as live — a lock is never
/// deleted unless it is provably stale.
fn is_stale(lock: &Path) -> bool {
    let modified = match fs::metadata(lock).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age > LOCK_STALE_AFTER,
        Err(_) => false,
    }
}

/// Secure credential read: names the path, never echoes content
/// (FR-4.6). Providers use it for the pre-lock read that decides
/// whether a refresh is needed.
pub fn read_json(path: &Path) -> Result<Value> {
    read_file_json(path, "reading")
}

/// Read and parse with a secret-free, verb-labelled diagnostic.
fn read_file_json(path: &Path, verb: &str) -> Result<Value> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("{verb} credential file {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("{verb} credential file {} as JSON", path.display()))
}

/// Acquire `<credential-filename>.llmu-refresh.lock` with
/// `create_new(true)` and contents `pid`, creation epoch in ms since
/// UNIX_EPOCH, and a per-attempt nonce (FR-4.7). Retries every
/// `retry_delay` until `timeout`; a lock provably older than
/// `LOCK_STALE_AFTER` is removed and `create_new` retried immediately.
/// The returned guard deletes the lock on release only when its nonce
/// still matches.
fn acquire_lock(path: &Path, timing: &LockTiming) -> Result<RefreshLock> {
    let lock = lock_path(path)?;
    let contents = format!(
        "{}\n{}\n{}\n",
        std::process::id(),
        now_ms(),
        NONCE.fetch_add(1, Ordering::Relaxed) + 1
    );
    let deadline = now_ms().saturating_add(timing.timeout.as_millis() as u64);
    loop {
        let retry_now = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(contents.as_bytes()).and_then(|()| f.sync_all()) {
                    let _ = fs::remove_file(&lock);
                    return Err(e)
                        .with_context(|| format!("writing refresh lock {}", lock.display()));
                }
                return Ok(RefreshLock {
                    path: lock,
                    contents,
                    armed: true,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale(&lock) {
                    // FR-4.7: remove the stale lock and retry create_new
                    // on the next iteration without sleeping.
                    fs::remove_file(&lock).is_ok()
                } else {
                    false
                }
            }
            Err(e) => {
                return Err(e).with_context(|| format!("creating refresh lock {}", lock.display()));
            }
        };
        if now_ms() >= deadline {
            bail!(
                "timed out waiting for refresh lock {} after {}ms — another llmu process may be refreshing this credential",
                lock.display(),
                timing.timeout.as_millis()
            );
        }
        if !retry_now {
            std::thread::sleep(timing.retry_delay);
        }
    }
}

#[cfg(unix)]
fn restrictive_mode(opts: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    // Mode is applied at creation so the temp file is 0600 from the
    // very first byte — there is no permissive window (FR-4.5).
    opts.mode(0o600);
}

#[cfg(not(unix))]
fn restrictive_mode(_opts: &mut fs::OpenOptions) {
    // Best effort on non-Unix platforms without mode bits.
}

/// Write a unique same-directory temp file (FR-4.5): a create_new name
/// in the target's directory keeps the later rename on one filesystem,
/// Unix mode `0600` is set at creation, and the data is flushed before
/// the path is returned. On any write failure the partial file is
/// removed.
fn write_temp(dir: &Path, data: &[u8]) -> Result<PathBuf> {
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        std::process::id(),
        now_ms(),
        NONCE.fetch_add(1, Ordering::Relaxed) + 1
    ));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    restrictive_mode(&mut opts);
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("creating credential temp file {}", tmp.display()))?;
    if let Err(e) = f.write_all(data).and_then(|()| f.sync_all()) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing credential temp file {}", tmp.display()));
    }
    Ok(tmp)
}

/// The guarded credential transaction (FR-4.2 through FR-4.6):
///
/// 1. Acquire the llmu refresh lock.
/// 2. Re-read under the lock. When the stored refresh token already
///    differs from `expected_refresh_token`, another process refreshed
///    first: return `ChangedByOther` with the fresh root and write
///    nothing (FR-4.3 adoption seam).
/// 3. Run the provider-owned `merge` on a fresh copy so every unknown
///    top-level and nested field survives, then validate (FR-4.2, FR-4.6).
/// 4. Write a unique same-directory `0600` temp file and flush it.
/// 5. Re-read the target and compare its refresh token to
///    `expected_refresh_token` (CAS, FR-4.4). Mismatch aborts with a
///    concurrency diagnostic and the temp file is removed.
/// 6. Rename over the target, then verify by re-reading the required
///    fields (FR-4.5).
///
/// Any HTTP, JSON, validation, CAS, or persistence failure leaves the
/// original file unchanged and never logs credential values (FR-4.6).
/// The CAS/rename pair is atomic within llmu's lock; writers that
/// ignore the llmu lock can still race the check/rename window — the
/// residual race is acknowledged, not hidden (AD-2).
pub fn replace_with_cas(
    timing: &LockTiming,
    path: &Path,
    schema: &CredentialSchema,
    expected_refresh_token: &str,
    merge: impl FnOnce(&Value) -> Value,
) -> Result<ReplaceOutcome> {
    let _lock = acquire_lock(path, timing)?;

    // FR-4.3: re-read under the lock. When the stored refresh token
    // already differs from the request token, another llmu process
    // refreshed first — hand back the fresh root so the caller can
    // adopt the new token and skip its duplicate refresh; write nothing.
    let current = read_file_json(path, "re-reading")?;
    let stored = (schema.refresh_token)(&current).context("no refresh token in credential file")?;
    if stored != expected_refresh_token {
        return Ok(ReplaceOutcome::ChangedByOther { value: current });
    }

    // FR-4.2: the provider-owned merge works on a fresh deep copy, so
    // every unknown top-level and nested field survives by construction.
    let merged = merge(&current);
    (schema.validate)(&merged).context("merged credential failed provider validation")?;

    // FR-4.5/4.6: validation precedes the temp file; then write a unique
    // same-directory 0600 temp file and flush it before any replacement.
    let mut data =
        serde_json::to_vec_pretty(&merged).context("encoding merged credential as JSON")?;
    data.push(b'\n');
    let dir = path
        .parent()
        .context("credential path has no parent directory")?;
    let tmp = write_temp(dir, &data)?;
    let outcome = (|| -> Result<ReplaceOutcome> {
        // FR-4.4: compare-and-swap immediately before replacement. The
        // target may have changed since our under-lock re-read — writers
        // that ignore the llmu lock can race this window, so the CAS
        // aborts rather than overwrites (residual race, AD-2).
        let fresh = read_file_json(path, "re-reading")?;
        match (schema.refresh_token)(&fresh) {
            Some(t) if t == expected_refresh_token => {}
            _ => bail!(
                "credential file changed by another writer; refresh aborted — no changes written"
            ),
        }
        fs::rename(&tmp, path)
            .with_context(|| format!("replacing credential file {}", path.display()))?;
        // FR-4.5: verify by re-reading the required fields.
        let on_disk = read_file_json(path, "re-reading")?;
        (schema.validate)(&on_disk).context("post-write verification of credential file failed")?;
        Ok(ReplaceOutcome::Replaced { value: on_disk })
    })();
    match outcome {
        Ok(o) => Ok(o),
        Err(e) => {
            // FR-4.6: remove the temp file on every failure path.
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    static TEST_DIR_NONCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let nonce = TEST_DIR_NONCE.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "llmu-cred-test-{}-{}-{nonce}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn default_timing() -> LockTiming {
        LockTiming::default()
    }

    fn short_timing() -> LockTiming {
        LockTiming {
            retry_delay: Duration::from_millis(10),
            timeout: Duration::from_millis(150),
        }
    }

    /// Generic test fixture field: the module must never know a
    /// provider's real schema, so tests use a neutral snake_case field.
    fn token(v: &Value) -> Option<&str> {
        v["refresh_token"].as_str()
    }

    fn always_ok(_v: &Value) -> Result<()> {
        Ok(())
    }

    fn schema<'a>(validate: &'a dyn Fn(&Value) -> Result<()>) -> CredentialSchema<'a> {
        CredentialSchema {
            refresh_token: &token,
            validate,
        }
    }

    fn write_creds(dir: &Path, obj: &Value) -> PathBuf {
        let p = dir.join("creds.json");
        fs::write(&p, serde_json::to_vec_pretty(obj).unwrap()).unwrap();
        p
    }

    fn temp_files(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().ends_with(".tmp"))
            .collect()
    }

    fn lock_path(cred: &Path) -> PathBuf {
        let name = cred.file_name().unwrap().to_string_lossy().into_owned();
        cred.with_file_name(format!("{name}{LOCK_SUFFIX}"))
    }

    #[test]
    fn merge_preserves_unknown_root_and_nested_fields() {
        let dir = temp_dir("preserve");
        let p = write_creds(
            &dir,
            &json!({
                "refresh_token": "T1",
                "unknown_root": {"deep": [1, 2, {"x": "y"}]},
                "meta": {"unknown_inner": "keep", "known": "old"}
            }),
        );
        let out = replace_with_cas(&default_timing(), &p, &schema(&always_ok), "T1", |v| {
            let mut m = v.clone();
            m["access_token"] = json!("NEW");
            m["meta"]["known"] = json!("new");
            m
        })
        .unwrap();
        assert!(
            matches!(out, ReplaceOutcome::Replaced { .. }),
            "expected a successful replace"
        );
        let on_disk = read_json(&p).unwrap();
        assert_eq!(
            on_disk["unknown_root"],
            json!({"deep": [1, 2, {"x": "y"}]}),
            "unknown root fields must survive (FR-4.2)"
        );
        assert_eq!(
            on_disk["meta"]["unknown_inner"], "keep",
            "unknown nested fields must survive (FR-4.2)"
        );
        assert_eq!(on_disk["meta"]["known"], "new");
        assert_eq!(on_disk["access_token"], "NEW");
        assert_eq!(on_disk["refresh_token"], "T1");
    }

    #[test]
    fn cas_success_replaces_with_verified_merged_value() {
        let dir = temp_dir("cas-ok");
        let p = write_creds(&dir, &json!({"refresh_token": "T1", "old": 1}));
        let out = replace_with_cas(&default_timing(), &p, &schema(&always_ok), "T1", |v| {
            let mut m = v.clone();
            m["access_token"] = json!("NEW");
            m["old"] = json!(2);
            m
        })
        .unwrap();
        let ReplaceOutcome::Replaced { value } = out else {
            panic!("expected replaced outcome");
        };
        assert_eq!(value["access_token"], "NEW");
        let on_disk = read_json(&p).unwrap();
        assert_eq!(
            on_disk, value,
            "the reported value must be the verified on-disk content"
        );
        assert_eq!(on_disk["old"], 2, "the old content was replaced atomically");
    }

    #[test]
    fn cas_mismatch_aborts_and_preserves_foreign_write() {
        let dir = temp_dir("cas-mismatch");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let err = replace_with_cas(&default_timing(), &p, &schema(&always_ok), "T1", |_v| {
            // Simulate a writer that ignores the llmu lock landing between
            // our under-lock re-read and the pre-replace CAS re-read: the
            // merge closure runs exactly in that window.
            fs::write(
                &p,
                serde_json::to_vec_pretty(&json!({"refresh_token": "T2", "access_token": "OTHER"}))
                    .unwrap(),
            )
            .unwrap();
            json!({"refresh_token": "T1", "access_token": "OURS"})
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("changed by another writer"),
            "mismatch must surface a concurrency diagnostic (FR-4.4): {err}"
        );
        let on_disk = read_json(&p).unwrap();
        assert_eq!(
            on_disk["refresh_token"], "T2",
            "the foreign write must be preserved"
        );
        assert_eq!(
            on_disk["access_token"], "OTHER",
            "our merged value must never land after a mismatch"
        );
        assert!(
            temp_files(&dir).is_empty(),
            "the temp file must be removed after the CAS failure (FR-4.6)"
        );
    }

    #[test]
    fn temp_names_are_unique_same_directory_and_hidden() {
        let dir = temp_dir("temp-unique");
        let a = write_temp(&dir, b"a").unwrap();
        let b = write_temp(&dir, b"b").unwrap();
        assert_ne!(a, b, "each attempt must get a unique temp name (FR-4.5)");
        assert_eq!(
            a.parent(),
            Some(dir.as_path()),
            "the temp must live in the target's directory for an atomic rename"
        );
        let na = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(na.starts_with('.'));
        assert!(na.ends_with(".tmp"));
        fs::remove_file(&a).unwrap();
        fs::remove_file(&b).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mode_0600_at_creation_and_after_replace() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let tmp = write_temp(&dir, b"data").unwrap();
        let mode = fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the temp file must be 0600 from creation — no permissive window (FR-4.5)"
        );
        fs::remove_file(&tmp).unwrap();
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        replace_with_cas(&default_timing(), &p, &schema(&always_ok), "T1", |v| {
            let mut m = v.clone();
            m["access_token"] = json!("N");
            m
        })
        .unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the renamed result must keep mode 0600 (FR-4.5)"
        );
    }

    #[test]
    fn flush_rename_and_post_read_verification_run() {
        let dir = temp_dir("verify");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let calls = Cell::new(0usize);
        let validate = |v: &Value| -> Result<()> {
            calls.set(calls.get() + 1);
            assert!(
                v["access_token"].is_string(),
                "validation must see the required field"
            );
            Ok(())
        };
        let out = replace_with_cas(&default_timing(), &p, &schema(&validate), "T1", |v| {
            let mut m = v.clone();
            m["access_token"] = json!("NEW");
            m
        })
        .unwrap();
        assert!(matches!(out, ReplaceOutcome::Replaced { .. }));
        assert_eq!(
            calls.get(),
            2,
            "validate must run before the temp write and again on the post-rename re-read (FR-4.5, FR-4.6)"
        );
        assert!(read_json(&p).unwrap()["access_token"].is_string());
    }

    #[test]
    fn validation_failure_leaves_original_unchanged_without_temp() {
        let dir = temp_dir("validate-fail");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let before = fs::read(&p).unwrap();
        let failing = |_v: &Value| -> Result<()> { bail!("missing provider-required field") };
        let err = replace_with_cas(&default_timing(), &p, &schema(&failing), "T1", |v| {
            let mut m = v.clone();
            m["access_token"] = json!("NEW");
            m
        })
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("missing provider-required field"),
            "validation failure must surface the provider-owned diagnostic, got: {err:#}"
        );
        assert_eq!(
            fs::read(&p).unwrap(),
            before,
            "validation failure must leave the original byte-identical (FR-4.6)"
        );
        assert!(
            temp_files(&dir).is_empty(),
            "validation runs before the temp file is created (FR-4.6)"
        );
    }

    #[test]
    fn lock_path_is_sibling_with_llmu_suffix_and_create_new_blocks() {
        let dir = temp_dir("lock-name");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let lock = lock_path(&p);
        assert_eq!(
            lock,
            dir.join(format!("creds.json{LOCK_SUFFIX}")),
            "lock path must be <credential-filename>.llmu-refresh.lock (FR-4.7)"
        );
        let g = acquire_lock(&p, &default_timing()).unwrap();
        assert!(lock.exists());
        let err = acquire_lock(&p, &short_timing()).unwrap_err();
        assert!(
            err.to_string().contains("timed out"),
            "a held lock must block a second llmu process (FR-4.7): {err}"
        );
        drop(g);
        assert!(!lock.exists(), "releasing must remove the lock");
        let _g2 = acquire_lock(&p, &default_timing()).unwrap();
    }

    #[test]
    fn lock_metadata_carries_pid_epoch_and_unique_nonce() {
        let dir = temp_dir("lock-meta");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let g = acquire_lock(&p, &default_timing()).unwrap();
        let raw = fs::read_to_string(lock_path(&p)).unwrap();
        let mut lines = raw.lines();
        let pid: u64 = lines.next().unwrap().parse().unwrap();
        let epoch: u64 = lines.next().unwrap().parse().unwrap();
        let nonce: u64 = lines.next().unwrap().parse().unwrap();
        assert_eq!(
            pid,
            u64::from(std::process::id()),
            "lock must carry the acquiring pid (FR-4.7)"
        );
        assert!(
            epoch <= now_ms() && epoch > now_ms().saturating_sub(5000),
            "lock must carry a creation epoch (FR-4.7)"
        );
        assert!(nonce > 0, "lock must carry a per-attempt nonce (FR-4.7)");
        drop(g);
        let g2 = acquire_lock(&p, &default_timing()).unwrap();
        let raw2 = fs::read_to_string(lock_path(&p)).unwrap();
        let nonce2: u64 = raw2.lines().nth(2).unwrap().parse().unwrap();
        assert_ne!(nonce, nonce2, "each attempt needs its own nonce");
        drop(g2);
    }

    #[test]
    fn lock_timing_defaults_retry_100ms_timeout_5s() {
        let d = LockTiming::default();
        assert_eq!(d.retry_delay, Duration::from_millis(100), "FR-4.7 retry");
        assert_eq!(d.timeout, Duration::from_secs(5), "FR-4.7 timeout");
    }

    #[test]
    fn stale_lock_older_than_60s_is_removed_and_reacquired() {
        let dir = temp_dir("stale");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let lock = lock_path(&p);
        fs::write(&lock, "99999\n111\nforeign\n").unwrap();
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        let mut times = fs::FileTimes::new();
        times = times.set_modified(SystemTime::now() - Duration::from_secs(61));
        f.set_times(times).unwrap();
        drop(f);
        let g = acquire_lock(&p, &default_timing()).unwrap();
        let raw = fs::read_to_string(&lock).unwrap();
        let pid: u64 = raw.lines().next().unwrap().parse().unwrap();
        assert_eq!(
            pid,
            u64::from(std::process::id()),
            "a stale foreign lock must be removed and re-created with our metadata (FR-4.7)"
        );
        assert!(raw.lines().nth(2).unwrap().parse::<u64>().unwrap() > 0);
        drop(g);
    }

    #[test]
    fn live_contention_timeout_is_bounded_and_fresh_foreign_lock_blocks() {
        let dir = temp_dir("contention");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let lock = lock_path(&p);
        let g = acquire_lock(&p, &default_timing()).unwrap();
        let start = Instant::now();
        let err = acquire_lock(&p, &short_timing()).unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "contention must time out, not hang (FR-4.7)"
        );
        assert!(err.to_string().contains("timed out"));
        drop(g);
        fs::write(&lock, "99999\n111\nforeign\n").unwrap();
        let err = acquire_lock(&p, &short_timing()).unwrap_err();
        assert!(
            err.to_string().contains("timed out"),
            "a fresh foreign lock must block until timeout, never be deleted (FR-4.7): {err}"
        );
        fs::remove_file(&lock).unwrap();
    }

    #[test]
    fn nonce_safe_drop_keeps_foreign_recreated_lock() {
        let dir = temp_dir("nonce-drop");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let lock = lock_path(&p);
        let g = acquire_lock(&p, &default_timing()).unwrap();
        fs::remove_file(&lock).unwrap();
        fs::write(&lock, format!("{}\n{}\n{}\n", 424242, now_ms(), 777)).unwrap();
        drop(g);
        assert!(
            lock.exists(),
            "dropping our guard must not delete a lock re-created by another process (FR-4.7)"
        );
        let raw = fs::read_to_string(&lock).unwrap();
        assert!(raw.contains("777"), "the foreign lock content must survive");
        fs::remove_file(&lock).unwrap();
    }

    #[test]
    fn changed_by_other_adopts_foreign_token_without_merge_or_write() {
        let dir = temp_dir("adopt");
        let p = write_creds(
            &dir,
            &json!({"refresh_token": "T2", "access_token": "ALREADY-NEW", "meta": {"k": "v"}}),
        );
        let before = fs::read(&p).unwrap();
        let out = replace_with_cas(&default_timing(), &p, &schema(&always_ok), "T1", |_v| {
            panic!("merge must not run when another process already changed the token")
        })
        .unwrap();
        let ReplaceOutcome::ChangedByOther { value } = out else {
            panic!("expected the duplicate-refresh adoption seam (FR-4.3)");
        };
        assert_eq!(value["refresh_token"], "T2");
        assert_eq!(value["access_token"], "ALREADY-NEW");
        assert_eq!(
            fs::read(&p).unwrap(),
            before,
            "adoption must never write the credential file"
        );
        assert!(temp_files(&dir).is_empty());
    }

    #[test]
    fn missing_refresh_token_is_an_error_and_writes_nothing() {
        let dir = temp_dir("no-token");
        let p = write_creds(&dir, &json!({"access_token": "x"}));
        let before = fs::read(&p).unwrap();
        let err = replace_with_cas(&default_timing(), &p, &schema(&always_ok), "T1", |v| {
            v.clone()
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("no refresh token"),
            "unsupported credentials must fail without mutation (FR-4.1): {err}"
        );
        assert_eq!(fs::read(&p).unwrap(), before);
        assert!(temp_files(&dir).is_empty());
    }

    #[test]
    fn errors_and_read_failures_never_expose_token_values() {
        let dir = temp_dir("no-secrets");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let err = replace_with_cas(&default_timing(), &p, &schema(&always_ok), "T1", |_v| {
            fs::write(
                &p,
                serde_json::to_vec_pretty(&json!({"refresh_token": "T2"})).unwrap(),
            )
            .unwrap();
            json!({"refresh_token": "T1"})
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("T1") && !msg.contains("T2"),
            "diagnostics must never carry token values (FR-4.6): {msg}"
        );

        let missing = read_json(&dir.join("missing.json")).unwrap_err();
        assert!(missing.to_string().contains("missing.json"));

        let bad = dir.join("bad.json");
        fs::write(&bad, "{not json").unwrap();
        let err = read_json(&bad).unwrap_err();
        assert!(err.to_string().contains("bad.json"));
        assert!(
            !err.to_string().contains("not json"),
            "read errors must name the path, never echo content"
        );
        let err = replace_with_cas(&default_timing(), &bad, &schema(&always_ok), "T1", |v| {
            v.clone()
        })
        .unwrap_err();
        assert!(
            !err.to_string().contains("not json"),
            "transaction read errors must not echo content"
        );
        assert!(temp_files(&dir).is_empty());
    }
}
