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
//! with upstream CLI lock files (FR-4.7). The lock is acquired by
//! `lock_and_read` and held for the WHOLE transaction — across the
//! provider's refresh HTTP request and the persistence that follows
//! (FR-4.3). Two llmu processes can therefore never use the same
//! refresh token concurrently: the second blocks on the lock until the
//! first has persisted or given up, then re-reads and adopts.
//!
//! Cross-tool safety rests on the refresh-token compare-and-swap
//! immediately before rename; the residual non-atomic check/rename race
//! against writers that ignore the llmu lock is acknowledged rather
//! than hidden (AD-2).

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

/// What a persistence attempt did.
#[derive(Debug)]
pub enum ReplaceOutcome {
    /// The merged value was validated, written, and re-read successfully.
    Replaced { value: Value },
    /// Immediately before the rename the stored refresh token differed
    /// from the locked snapshot's token: a writer that ignores the llmu
    /// lock landed during this transaction. Nothing was overwritten;
    /// `value` is the freshly re-read root so the caller can adopt it
    /// and decide (AS-4).
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
/// whether a refresh is worth attempting.
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

/// Acquire the llmu refresh lock and re-read the credential file under
/// it (FR-4.3). The returned `LockedCredential` owns the guard for its
/// whole lifetime: a provider runs its refresh HTTP request while it
/// lives, inspects the snapshot with `value()`, and then either drops
/// it (an already-usable token needs no write) or consumes it with
/// `replace_with_cas` to persist through the SAME guard. Because the
/// lock is held before any provider work, a request-before-lock hazard
/// is unrepresentable — two llmu processes can never refresh the same
/// token concurrently.
pub fn lock_and_read(timing: &LockTiming, path: &Path) -> Result<LockedCredential> {
    let lock = acquire_lock(path, timing)?;
    let value = read_file_json(path, "re-reading")?;
    Ok(LockedCredential {
        _lock: lock,
        path: path.to_path_buf(),
        value,
    })
}

/// A credential file read while the llmu refresh lock is held.
#[derive(Debug)]
pub struct LockedCredential {
    _lock: RefreshLock,
    path: PathBuf,
    value: Value,
}

impl LockedCredential {
    /// The parsed root object captured under the lock.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// Persist a provider-merged value through the same guard (FR-4.2
    /// through FR-4.6). The expected refresh token is derived from the
    /// locked snapshot, never from a caller-supplied value, so a stale
    /// request token cannot be passed in by accident.
    ///
    /// Sequence: provider-owned `merge` on a fresh copy (unknown fields
    /// survive), validate, unique same-directory `0600` temp write with
    /// flush, CAS re-read immediately before rename, rename, post-rename
    /// re-read and validate. A cross-tool writer that lands in the
    /// check/rename window yields `ChangedByOther` with the fresh root
    /// and nothing is overwritten. Any failure removes the temp file and
    /// leaves the original unchanged; credential values never reach
    /// diagnostics (FR-4.6).
    pub fn replace_with_cas(
        self,
        schema: &CredentialSchema,
        merge: impl FnOnce(&Value) -> Value,
    ) -> Result<ReplaceOutcome> {
        // The CAS expected value comes from the snapshot read under the
        // lock — the caller cannot supply a pre-lock token that no
        // longer matches (FR-4.3, FR-4.4).
        let expected =
            (schema.refresh_token)(&self.value).context("no refresh token in credential file")?;

        // FR-4.2: the provider-owned merge works on a fresh deep copy, so
        // every unknown top-level and nested field survives by construction.
        let merged = merge(&self.value);
        (schema.validate)(&merged).context("merged credential failed provider validation")?;

        // FR-4.5/4.6: validation precedes the temp file; then write a unique
        // same-directory 0600 temp file and flush it before any replacement.
        let mut data =
            serde_json::to_vec_pretty(&merged).context("encoding merged credential as JSON")?;
        data.push(b'\n');
        let dir = self
            .path
            .parent()
            .context("credential path has no parent directory")?;
        let tmp = write_temp(dir, &data)?;
        let outcome = (|| -> Result<ReplaceOutcome> {
            // FR-4.4: compare-and-swap immediately before replacement. The
            // target may have changed since `lock_and_read` — writers that
            // ignore the llmu lock can race this window, so the CAS
            // returns `ChangedByOther` rather than overwriting (residual
            // race, AD-2; AS-4 adoption).
            let fresh = read_file_json(&self.path, "re-reading")?;
            match (schema.refresh_token)(&fresh) {
                Some(t) if t == expected => {}
                _ => return Ok(ReplaceOutcome::ChangedByOther { value: fresh }),
            }
            fs::rename(&tmp, &self.path)
                .with_context(|| format!("replacing credential file {}", self.path.display()))?;
            // FR-4.5: verify by re-reading the required fields.
            let on_disk = read_file_json(&self.path, "re-reading")?;
            (schema.validate)(&on_disk)
                .context("post-write verification of credential file failed")?;
            Ok(ReplaceOutcome::Replaced { value: on_disk })
        })();
        match outcome {
            Ok(o) => {
                if !matches!(o, ReplaceOutcome::Replaced { .. }) {
                    // FR-4.6: no write happened — remove the temp file.
                    let _ = fs::remove_file(&tmp);
                }
                Ok(o)
            }
            Err(e) => {
                // FR-4.6: remove the temp file on every failure path.
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
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
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        let out = locked.replace_with_cas(&schema(&always_ok), |v| {
            let mut m = v.clone();
            m["access_token"] = json!("NEW");
            m["meta"]["known"] = json!("new");
            m
        });
        let out = out.unwrap();
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
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        let out = locked
            .replace_with_cas(&schema(&always_ok), |v| {
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
    fn cross_tool_pre_rename_mismatch_returns_changed_by_other_without_overwrite() {
        let dir = temp_dir("cross-tool");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        // The provider's refresh window: a writer that ignores the llmu
        // lock rewrites the file while the guard is alive.
        fs::write(
            &p,
            serde_json::to_vec_pretty(&json!({"refresh_token": "T2", "access_token": "OTHER"}))
                .unwrap(),
        )
        .unwrap();
        let out = locked
            .replace_with_cas(&schema(&always_ok), |v| {
                let mut m = v.clone();
                m["access_token"] = json!("OURS");
                m
            })
            .unwrap();
        let ReplaceOutcome::ChangedByOther { value } = out else {
            panic!("a cross-tool token change must never be overwritten (AS-4)");
        };
        assert_eq!(
            value["refresh_token"], "T2",
            "the fresh root is offered for adoption"
        );
        assert_eq!(value["access_token"], "OTHER");
        let on_disk = read_json(&p).unwrap();
        assert_eq!(
            on_disk["refresh_token"], "T2",
            "the cross-tool write must be preserved"
        );
        assert_eq!(
            on_disk["access_token"], "OTHER",
            "our merged value must never land after a mismatch"
        );
        assert!(
            temp_files(&dir).is_empty(),
            "the temp file must be removed after the CAS mismatch (FR-4.6)"
        );
        assert!(
            !lock_path(&p).exists(),
            "consuming the guard must release the lock"
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
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        locked
            .replace_with_cas(&schema(&always_ok), |v| {
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
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        let out = locked
            .replace_with_cas(&schema(&validate), |v| {
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
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        let err = locked
            .replace_with_cas(&schema(&failing), |v| {
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
        assert!(
            !lock_path(&p).exists(),
            "consuming the guard must release the lock"
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
    fn lock_and_read_acquires_lock_and_returns_locked_snapshot() {
        let dir = temp_dir("lock-and-read");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let lock = lock_path(&p);
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        assert!(
            lock.exists(),
            "lock_and_read must hold the llmu lock for the whole transaction (FR-4.3)"
        );
        assert_eq!(
            locked.value()["refresh_token"],
            "T1",
            "the snapshot is the re-read taken under the lock"
        );
        let err = lock_and_read(&short_timing(), &p).unwrap_err();
        assert!(
            err.to_string().contains("timed out"),
            "a second local refresher must be excluded while the guard is alive (FR-4.3): {err}"
        );
        drop(locked);
        assert!(!lock.exists(), "dropping the guard must release the lock");
        let _again = lock_and_read(&default_timing(), &p).unwrap();
    }

    #[test]
    fn provider_inspects_current_json_while_guard_alive() {
        let dir = temp_dir("inspect");
        let p = write_creds(
            &dir,
            &json!({"refresh_token": "T1", "meta": {"unknown_inner": "keep"}}),
        );
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        let on_disk: Value =
            serde_json::from_slice(&fs::read(&p).unwrap()).expect("fixture parses");
        assert_eq!(
            locked.value(),
            &on_disk,
            "the provider must see the current JSON while the guard is alive"
        );
        assert!(lock_path(&p).exists(), "the guard is still held");
    }

    #[test]
    fn second_refresher_cannot_enter_network_section_until_first_guard_drops() {
        let dir = temp_dir("network-section");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let guard = lock_and_read(&default_timing(), &p).unwrap();
        let entered = Arc::new(AtomicBool::new(false));
        let entered2 = entered.clone();
        let path2 = p.clone();
        let handle = std::thread::spawn(move || {
            let locked = lock_and_read(
                &LockTiming {
                    retry_delay: Duration::from_millis(10),
                    timeout: Duration::from_secs(1),
                },
                &path2,
            )
            .expect("second refresher acquires once the first guard drops");
            // The simulated network-request section is only reachable
            // after lock_and_read returns.
            entered2.store(true, Ordering::SeqCst);
            drop(locked);
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !entered.load(Ordering::SeqCst),
            "the second refresher must not reach its network section while the first guard is alive (FR-4.3)"
        );
        drop(guard);
        handle.join().unwrap();
        assert!(
            entered.load(Ordering::SeqCst),
            "the second refresher must reach its network section after the first guard drops"
        );
        assert!(!lock_path(&p).exists(), "both guards are released");
    }

    #[test]
    fn persistence_consumes_the_same_guard_without_reacquiring() {
        let dir = temp_dir("same-guard");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let lock = lock_path(&p);
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        assert!(
            lock.exists(),
            "the guard holds the lock across the simulated refresh HTTP window"
        );
        let out = locked
            .replace_with_cas(&schema(&always_ok), |v| {
                let mut m = v.clone();
                m["access_token"] = json!("NEW");
                m
            })
            .unwrap();
        assert!(
            matches!(out, ReplaceOutcome::Replaced { .. }),
            "persistence through the same guard must succeed without re-acquiring the lock (re-acquisition would time out against its own lock file)"
        );
        assert!(
            !lock.exists(),
            "consuming the guard releases the lock only after persistence"
        );
    }

    #[test]
    fn already_usable_credentials_can_be_dropped_without_http_or_write() {
        let dir = temp_dir("usable-drop");
        let p = write_creds(
            &dir,
            &json!({"refresh_token": "T2", "access_token": "ALREADY-NEW", "meta": {"k": "v"}}),
        );
        let before = fs::read(&p).unwrap();
        let lock = lock_path(&p);
        {
            let locked = lock_and_read(&default_timing(), &p).unwrap();
            let snapshot = locked.value();
            assert_eq!(snapshot["refresh_token"], "T2");
            assert_eq!(snapshot["access_token"], "ALREADY-NEW");
            // Provider decides the token is already usable: no refresh
            // HTTP, no persistence — dropping the guard is the whole cost.
        }
        assert_eq!(
            fs::read(&p).unwrap(),
            before,
            "dropping without write must leave the file byte-identical"
        );
        assert!(!lock.exists(), "dropping must release the llmu lock");
        assert!(temp_files(&dir).is_empty());
    }

    #[test]
    fn missing_refresh_token_is_an_error_and_writes_nothing() {
        let dir = temp_dir("no-token");
        let p = write_creds(&dir, &json!({"access_token": "x"}));
        let before = fs::read(&p).unwrap();
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        let err = locked
            .replace_with_cas(&schema(&always_ok), |v| v.clone())
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("no refresh token"),
            "credentials without the provider-required refresh field must fail without mutation (FR-4.1): {err:#}"
        );
        assert_eq!(fs::read(&p).unwrap(), before);
        assert!(temp_files(&dir).is_empty());
        assert!(
            !lock_path(&p).exists(),
            "consuming the guard must release the lock"
        );
    }

    #[test]
    fn errors_and_read_failures_never_expose_token_values() {
        let dir = temp_dir("no-secrets");
        let p = write_creds(&dir, &json!({"refresh_token": "T1"}));
        let failing = |_v: &Value| -> Result<()> { bail!("validation failed") };
        let locked = lock_and_read(&default_timing(), &p).unwrap();
        let err = locked
            .replace_with_cas(&schema(&failing), |v| {
                let mut m = v.clone();
                m["access_token"] = json!("TOK-1");
                m
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("T1") && !msg.contains("TOK-1"),
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
        let locked = lock_and_read(&default_timing(), &bad).unwrap_err();
        assert!(
            !locked.to_string().contains("not json"),
            "transaction read errors must not echo content"
        );
        assert!(temp_files(&dir).is_empty());
    }
}
