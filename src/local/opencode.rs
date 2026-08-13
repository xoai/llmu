//! OpenCode local usage collector — foundation layer (Task 2).
//!
//! Safely identifies the live database at
//! `<discover::opencode_data_dir()>/opencode.db` (FR-3), probes metadata
//! before opening (NotFound is normal absence, any other failure is
//! unreadable), opens strictly READONLY|NOMUTEX (FR-5), bounds busy
//! waits to 250 ms (FR-6, NFR-3), forces `query_only` ON and verifies
//! it, then streams exactly one time-bounded `message` query through
//! the rusqlite row iterator (FR-9/10/11/12). Record parsing and
//! normalization land in Task 3; this layer exposes the row seam and
//! classifies failures into bounded, secret-free categories (FR-8).
//! Nothing here creates, writes, copies, or snapshots the database
//! (FR-6/7, NFR-5), and notes are fixed literals: no raw rusqlite
//! error, path, SQL, JSON, id, prompt, or credential can reach them
//! (FR-8, NFR-6/7).

use crate::config::EnvLookup;
use crate::local::Collected;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bounded, secret-free database failure categories (FR-8). Raw
/// rusqlite errors, paths, SQL, JSON, ids, prompts, and credentials
/// never surface in notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbError {
    /// No `opencode.db` — normal absence (FR-3): silent, no events, no note.
    Missing,
    /// A metadata/open/query failure that is neither absence nor a
    /// bounded busy wait nor a schema mismatch.
    Unreadable,
    /// The database stayed locked past the 250 ms busy bound (NFR-3).
    Busy,
    /// The database opens but the expected `message` surface is absent.
    Incompatible,
}

/// One streamed row of the `message` seam (FR-9/FR-10): only the two
/// selected fields; the ordering `id` never leaves the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    /// UTC epoch milliseconds (FR-10).
    pub time_created_ms: i64,
    /// Raw `message.data` JSON text; parsing is Task 3.
    pub data: String,
}

/// The collector's single query (FR-9/FR-10/FR-11/FR-12): only the
/// `message` table, only `time_created` and `data`, exact `[since,
/// until)` bounds in UTC epoch milliseconds, deterministic order by
/// time then id.
const MESSAGE_QUERY: &str = "SELECT time_created, data FROM message \
     WHERE time_created >= ?1 AND time_created < ?2 \
     ORDER BY time_created, id";

/// Map a rusqlite failure to a bounded category (FR-8). Internal only:
/// the raw error is never emitted. Covers both the open/query variants
/// (`SqliteFailure`) and statement-preparation variants
/// (`SqlInputError`, which carries no meaningful code for missing
/// columns), so schema mismatches classify consistently.
fn classify(err: &rusqlite::Error) -> DbError {
    let (code, msg) = match err {
        rusqlite::Error::SqliteFailure(ffi_err, msg) => (Some(ffi_err.code), msg.as_deref()),
        rusqlite::Error::SqlInputError { error, msg, .. } => {
            (Some(error.code), Some(msg.as_str()))
        }
        _ => (None, None),
    };
    match code {
        Some(
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked,
        ) => DbError::Busy,
        Some(rusqlite::ErrorCode::NotADatabase) => DbError::Unreadable,
        _ if msg.is_some_and(|m| m.contains("no such table") || m.contains("no such column")) => {
            DbError::Incompatible
        }
        _ => DbError::Unreadable,
    }
}

/// Live database path via the shared resolver (FR-3): the effective
/// OpenCode data dir joined with `opencode.db`. Resolving never creates
/// anything; an unresolvable dir simply yields no path.
pub fn db_path(env: EnvLookup, home: Option<&Path>) -> Option<PathBuf> {
    crate::discover::opencode_data_dir(env, home).map(|d| d.join("opencode.db"))
}

#[allow(dead_code)] // std entry chain; wired by gather/CLI in Task 5
fn std_env(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[allow(dead_code)] // std entry chain; wired by gather/CLI in Task 5
fn std_db_path() -> Option<PathBuf> {
    db_path(&std_env, dirs::home_dir().as_deref())
}

/// Metadata probe before open (FR-3): a NotFound is normal absence;
/// any other metadata failure is unreadable. Existence never authorizes
/// creation.
fn metadata_class(path: &Path) -> Result<(), DbError> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(DbError::Missing),
        Err(_) => Err(DbError::Unreadable),
    }
}

/// Open the live database strictly read-only (FR-5): exactly
/// READONLY|NOMUTEX, busy timeout set immediately to 250 ms (FR-6,
/// NFR-3), `query_only` forced ON and verified (FR-6). No other pragma,
/// no DDL, no copy, no URI, no frozen-mode open (FR-6/7).
pub fn open_readonly(path: &Path) -> Result<Connection, DbError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| classify(&e))?;
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(|e| classify(&e))?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|e| classify(&e))?;
    let on: bool = conn
        .pragma_query_value(None, "query_only", |row| row.get(0))
        .map_err(|e| classify(&e))?;
    if on {
        Ok(conn)
    } else {
        Err(DbError::Unreadable)
    }
}

/// Streaming row seam (FR-11/FR-12): one prepared statement, one pass
/// over the exact `[since_ms, until_ms)` window, one row at a time via
/// the rusqlite row iterator. `on_row` receives each row in
/// `(time_created, id)` order; rows are never materialized. Preparing
/// the query doubles as the schema check: a database without the
/// expected surface classifies as Incompatible (FR-8).
pub fn stream_message_rows(
    conn: &Connection,
    since_ms: i64,
    until_ms: i64,
    mut on_row: impl FnMut(MessageRow),
) -> Result<(), DbError> {
    let mut stmt = conn.prepare(MESSAGE_QUERY).map_err(|e| classify(&e))?;
    let mut rows = stmt.query(rusqlite::params![since_ms, until_ms]).map_err(|e| classify(&e))?;
    while let Some(row) = rows.next().map_err(|e| classify(&e))? {
        on_row(MessageRow {
            time_created_ms: row.get::<_, i64>(0).map_err(|e| classify(&e))?,
            data: row.get::<_, String>(1).map_err(|e| classify(&e))?,
        });
    }
    Ok(())
}

const NOTE_UNREADABLE: &str = "opencode: database unreadable (local usage skipped)";
const NOTE_BUSY: &str = "opencode: database busy (local usage skipped)";
const NOTE_INCOMPATIBLE: &str = "opencode: database schema incompatible (local usage skipped)";

/// Collect from a concrete database path (FR-3/FR-25). Hermetic core:
/// the path is injected so tests run against synthetic databases only
/// (NFR-8); the std entry point resolves the live path. Events stay
/// empty until Task 3 normalization (FR-25); failures emit only the
/// fixed, secret-free notes above (FR-8/FR-34).
pub fn collect_from(path: &Path, since: DateTime<Utc>, until: DateTime<Utc>) -> Collected {
    let mut notes: Vec<String> = vec![];
    match metadata_class(path) {
        Ok(()) => {}
        Err(DbError::Missing) => {
            return Collected { events: vec![], notes };
        }
        Err(_) => {
            notes.push(NOTE_UNREADABLE.to_string());
            return Collected { events: vec![], notes };
        }
    }
    let conn = match open_readonly(path) {
        Ok(conn) => conn,
        Err(DbError::Busy) => {
            notes.push(NOTE_BUSY.to_string());
            return Collected { events: vec![], notes };
        }
        Err(_) => {
            notes.push(NOTE_UNREADABLE.to_string());
            return Collected { events: vec![], notes };
        }
    };
    let since_ms = since.timestamp_millis();
    let until_ms = until.timestamp_millis();
    match stream_message_rows(&conn, since_ms, until_ms, |_| {}) {
        Ok(()) => {}
        Err(DbError::Busy) => notes.push(NOTE_BUSY.to_string()),
        Err(DbError::Incompatible) => notes.push(NOTE_INCOMPATIBLE.to_string()),
        Err(_) => notes.push(NOTE_UNREADABLE.to_string()),
    }
    Collected { events: vec![], notes }
}

/// Std entry point (FR-3): resolves the live database through the
/// shared resolver and defers to `collect_from`.
#[allow(dead_code)] // not yet reachable; gather/CLI wiring lands in Task 5
pub fn collect(since: DateTime<Utc>, until: DateTime<Utc>) -> Collected {
    match std_db_path() {
        Some(path) => collect_from(&path, since, until),
        None => Collected {
            events: vec![],
            notes: vec![],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use rusqlite::Connection;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::path::Path;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "llmu-opencode-t2-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Synthetic OpenCode `message` table (FR-9 shape) with the given
    /// (time_created_ms, id, data) rows; returns the database path.
    fn write_db(dir: &Path, rows: &[(i64, &str, &str)]) -> PathBuf {
        let path = dir.join("opencode.db");
        let w = Connection::open(&path).unwrap();
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

    fn ms(t: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(t).unwrap()
    }

    fn no_env(_k: &str) -> Option<String> {
        None
    }

    fn env_pairs<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    fn snapshot(path: &Path) -> (u64, u64, SystemTime) {
        let bytes = std::fs::read(path).unwrap();
        let mut h = DefaultHasher::new();
        bytes.hash(&mut h);
        (
            bytes.len() as u64,
            h.finish(),
            std::fs::metadata(path).unwrap().modified().unwrap(),
        )
    }

    #[test]
    fn db_path_joins_opencode_db_after_shared_resolver() {
        let home = Path::new("/tmp/llmu-opencode-t2-home");
        assert_eq!(
            db_path(&env_pairs(&[("XDG_DATA_HOME", "/tmp/llmu-xdg")]), Some(home)),
            Some(PathBuf::from("/tmp/llmu-xdg/opencode/opencode.db")),
            "XDG_DATA_HOME resolves to <xdg>/opencode/opencode.db (FR-3)"
        );
        assert_eq!(
            db_path(&env_pairs(&[("OPENCODE_DATA_DIR", "/tmp/llmu-oc")]), None),
            Some(PathBuf::from("/tmp/llmu-oc/opencode.db")),
            "OPENCODE_DATA_DIR resolves to <dir>/opencode.db (FR-3)"
        );
        assert_eq!(
            db_path(&no_env, Some(home)),
            Some(PathBuf::from(
                "/tmp/llmu-opencode-t2-home/.local/share/opencode/opencode.db"
            )),
            "the home fallback resolves to ~/.local/share/opencode/opencode.db (FR-3)"
        );
        assert_eq!(
            db_path(&no_env, None),
            None,
            "no data dir resolves -> no database path (FR-3)"
        );
    }

    #[test]
    fn collect_missing_db_is_silent_with_no_events_or_notes() {
        let dir = temp_dir("missing");
        let out = collect_from(&dir.join("opencode.db"), ms(0), ms(1000));
        assert!(
            out.events.is_empty(),
            "no database -> no events (FR-3/FR-34)"
        );
        assert!(
            out.notes.is_empty(),
            "missing database is silent, never a note (FR-3): {:?}",
            out.notes
        );
    }

    #[test]
    fn collect_metadata_failure_other_than_notfound_is_unreadable() {
        let dir = temp_dir("meta");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let out = collect_from(&blocker.join("opencode.db"), ms(0), ms(1000));
        assert!(out.events.is_empty());
        assert_eq!(
            out.notes.len(),
            1,
            "a non-NotFound metadata failure is categorized (FR-3)"
        );
        assert!(
            out.notes[0].contains("unreadable"),
            "category: {:?}",
            out.notes[0]
        );
        assert!(
            !out.notes[0].contains("blocker"),
            "notes never carry paths (FR-8): {:?}",
            out.notes[0]
        );
    }

    #[test]
    fn collect_garbage_db_file_is_unreadable_with_bounded_note() {
        let dir = temp_dir("garbage");
        let path = dir.join("opencode.db");
        std::fs::write(&path, "this is definitely not a sqlite database").unwrap();
        let out = collect_from(&path, ms(0), ms(1000));
        assert!(out.events.is_empty());
        assert_eq!(out.notes.len(), 1);
        assert!(out.notes[0].contains("unreadable"));
        assert!(
            !out.notes[0].contains("not a sqlite"),
            "raw failure text never leaks (FR-8): {:?}",
            out.notes[0]
        );
    }

    #[test]
    fn open_readonly_enables_query_only_and_rejects_writes() {
        let path = write_db(&temp_dir("queryonly"), &[(100, "m1", "{}")]);
        let conn = open_readonly(&path).expect("read-only open succeeds");
        let qo: bool = conn
            .pragma_query_value(None, "query_only", |r| r.get(0))
            .unwrap();
        assert!(qo, "query_only must be ON after open (FR-6)");
        assert!(
            conn.execute_batch("CREATE TABLE should_fail (x INTEGER)")
                .is_err(),
            "query_only must reject state-changing statements (FR-6)"
        );
    }

    #[test]
    fn stream_message_rows_honors_exact_bounds_and_field_selection() {
        let path = write_db(
            &temp_dir("bounds"),
            &[
                (100, "m1", r#"{"text":"one"}"#),
                (200, "m2", r#"{"text":"two"}"#),
                (300, "m3", r#"{"text":"three"}"#),
                (400, "m4", r#"{"text":"four"}"#),
            ],
        );
        let conn = open_readonly(&path).unwrap();
        let mut got = vec![];
        stream_message_rows(&conn, 150, 350, |r| got.push(r)).unwrap();
        assert_eq!(
            got.len(),
            2,
            "exact [since, until) window excludes 100 and 400 (FR-10)"
        );
        assert_eq!(got[0].time_created_ms, 200);
        assert_eq!(got[0].data, r#"{"text":"two"}"#);
        assert_eq!(got[1].time_created_ms, 300);
        assert_eq!(got[1].data, r#"{"text":"three"}"#);
        let mut none = vec![];
        stream_message_rows(&conn, 400, 400, |r| none.push(r)).unwrap();
        assert!(none.is_empty(), "an empty half-open window yields no rows (FR-10)");
    }

    #[test]
    fn stream_message_rows_orders_by_time_then_id() {
        let path = write_db(
            &temp_dir("order"),
            &[
                (200, "row-b", r#"{"x":1}"#),
                (100, "row-0", r#"{"x":2}"#),
                (200, "row-a", r#"{"x":3}"#),
            ],
        );
        let conn = open_readonly(&path).unwrap();
        let mut got = vec![];
        stream_message_rows(&conn, 0, 1000, |r| got.push(r)).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].time_created_ms, 100);
        assert_eq!(
            got[1].data, r#"{"x":3}"#,
            "same-time rows order by id (row-a before row-b) (FR-10)"
        );
        assert_eq!(got[2].data, r#"{"x":1}"#);
    }

    #[test]
    fn stream_message_rows_classifies_incompatible_schema() {
        let dir = temp_dir("incompat");
        let empty_db = dir.join("empty.db");
        Connection::open(&empty_db).unwrap();
        let conn = open_readonly(&empty_db).expect("an empty database still opens");
        let mut n = 0;
        assert_eq!(
            stream_message_rows(&conn, 0, 1000, |_| n += 1),
            Err(DbError::Incompatible),
            "a database without the message table is incompatible (FR-8)"
        );
        let narrow = dir.join("narrow.db");
        let w = Connection::open(&narrow).unwrap();
        w.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, time_created INTEGER, role TEXT)",
        )
        .unwrap();
        drop(w);
        let conn = open_readonly(&narrow).unwrap();
        assert_eq!(
            stream_message_rows(&conn, 0, 1000, |_| n += 1),
            Err(DbError::Incompatible),
            "a message table missing `data` is incompatible (FR-8)"
        );
    }

    #[test]
    fn stream_message_rows_busy_waits_250ms_then_classifies() {
        let path = write_db(&temp_dir("busy"), &[(100, "m1", "{}")]);
        let w = Connection::open(&path).unwrap();
        w.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let conn = open_readonly(&path).expect("open succeeds; pragmas take no lock");
        let start = Instant::now();
        let mut n = 0;
        let res = stream_message_rows(&conn, 0, 1000, |_| n += 1);
        let elapsed = start.elapsed();
        assert_eq!(
            res,
            Err(DbError::Busy),
            "a locked database maps to the busy category (FR-8)"
        );
        assert!(
            elapsed >= Duration::from_millis(250),
            "the busy handler must wait the full 250 ms bound (NFR-3): {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "the busy wait stays bounded: {elapsed:?}"
        );
        assert_eq!(n, 0, "no row is delivered under a busy database (FR-34)");
    }

    #[test]
    fn collect_roundtrip_streams_readonly_without_events_or_notes() {
        let path = write_db(&temp_dir("roundtrip"), &[(100, "m1", "{}"), (500, "m2", "{}")]);
        let out = collect_from(&path, ms(0), ms(1000));
        assert!(
            out.events.is_empty(),
            "normalization belongs to Task 3 (FR-25)"
        );
        assert!(
            out.notes.is_empty(),
            "a readable database yields no notes: {:?}",
            out.notes
        );
    }

    #[test]
    fn collect_classifies_incompatible_schema_with_bounded_note() {
        let dir = temp_dir("collect-incompat");
        let empty_db = dir.join("opencode.db");
        Connection::open(&empty_db).unwrap();
        let out = collect_from(&empty_db, ms(0), ms(1000));
        assert!(out.events.is_empty());
        assert_eq!(out.notes.len(), 1);
        assert!(
            out.notes[0].contains("incompatible"),
            "note: {:?}",
            out.notes[0]
        );
        assert!(
            !out.notes[0].contains("opencode.db"),
            "notes never carry paths (FR-8): {:?}",
            out.notes[0]
        );
    }

    #[test]
    fn collect_never_touches_db_bytes_hash_or_mtime() {
        let path = write_db(
            &temp_dir("nfr5"),
            &[(100, "m1", r#"{"text":"a"}"#), (200, "m2", r#"{"text":"b"}"#)],
        );
        let before = snapshot(&path);
        let out = collect_from(&path, ms(0), ms(1000));
        assert!(out.events.is_empty());
        assert!(out.notes.is_empty());
        let after = snapshot(&path);
        assert_eq!(
            before, after,
            "read-only collect leaves length, hash, and mtime unchanged (NFR-5)"
        );
    }
}
