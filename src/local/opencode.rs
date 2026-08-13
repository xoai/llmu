//! OpenCode local usage collector.
//!
//! Safely identifies the live database at
//! `<discover::opencode_data_dir()>/opencode.db` (FR-3), probes metadata
//! before opening (NotFound is normal absence, any other failure is
//! unreadable), opens strictly READONLY|NOMUTEX (FR-5), bounds busy
//! waits to 250 ms (FR-6, NFR-3), forces `query_only` ON and verifies
//! it, then streams exactly one time-bounded `message` query through
//! the rusqlite row iterator (FR-9/10/11/12). Completed assistant
//! records are strictly validated, mapped through an explicit provider
//! allowlist, normalized, and aggregated into hourly buckets (FR-13-22).
//! Task 4 adds a bounded availability probe (`available_from` /
//! `available`) that answers `llmu providers` status through one
//! connection-local scalar reusing the exact shared strict decoder and
//! record predicate; it never runs the collector.
//! Nothing here creates, writes, copies, or snapshots the database
//! (FR-6/7, NFR-5). Database notes are fixed literals and record notes
//! interpolate counts only: no raw rusqlite error, path, SQL, JSON, id,
//! prompt, or credential can reach them (FR-8, NFR-6/7).

use crate::config::{Config, EnvLookup};
use crate::local::Collected;
use crate::types::{SourceKind, UsageEvent};
use chrono::{DateTime, Timelike, Utc};
use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;
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
    /// Raw `message.data` JSON text, present only when the SQLite
    /// storage class is TEXT with valid UTF-8; BLOB/INTEGER/REAL/NULL
    /// and invalid-UTF-8 TEXT surface as `None` and are counted
    /// malformed, never a database failure (FR-14/FR-34).
    pub data: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRecord {
    provider_id: String,
    model: String,
    created: DateTime<Utc>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
}

/// Strict serde `Deserialize` wrapper: builds the same
/// `serde_json::Value` the collector reads, but rejects duplicate
/// decoded object member names at ANY nesting depth before overwrite
/// (FR-14). `serde_json::Value` alone silently keeps the last member.
/// Equality is exact decoded Rust `String` equality — no Unicode
/// normalization or case folding — so escaped/literal equivalents and
/// embedded-NUL-equal names duplicate, while the same name in
/// different object instances stays valid. Number, Unicode, recursion,
/// and validity semantics are serde_json's own (u64 through
/// `u64::MAX`, `-0` and 2^64 rejected, lone surrogates rejected,
/// recursion limit preserved).
#[derive(Debug, Clone, PartialEq)]
struct StrictValue(serde_json::Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictVisitor;

        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_bool<E>(self, v: bool) -> Result<StrictValue, E> {
                Ok(StrictValue(serde_json::Value::Bool(v)))
            }

            fn visit_i64<E>(self, v: i64) -> Result<StrictValue, E> {
                Ok(StrictValue(serde_json::Value::Number(v.into())))
            }

            fn visit_u64<E>(self, v: u64) -> Result<StrictValue, E> {
                Ok(StrictValue(serde_json::Value::Number(v.into())))
            }

            fn visit_f64<E>(self, v: f64) -> Result<StrictValue, E>
            where
                E: de::Error,
            {
                serde_json::Number::from_f64(v)
                    .map(|n| StrictValue(serde_json::Value::Number(n)))
                    .ok_or_else(|| de::Error::custom("invalid JSON number"))
            }

            fn visit_str<E>(self, v: &str) -> Result<StrictValue, E> {
                Ok(StrictValue(serde_json::Value::String(v.to_string())))
            }

            fn visit_string<E>(self, v: String) -> Result<StrictValue, E> {
                Ok(StrictValue(serde_json::Value::String(v)))
            }

            fn visit_unit<E>(self) -> Result<StrictValue, E> {
                Ok(StrictValue(serde_json::Value::Null))
            }

            fn visit_none<E>(self) -> Result<StrictValue, E> {
                Ok(StrictValue(serde_json::Value::Null))
            }

            fn visit_some<D>(self, deserializer: D) -> Result<StrictValue, D::Error>
            where
                D: Deserializer<'de>,
            {
                deserializer.deserialize_any(StrictVisitor)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<StrictValue, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(1024));
                while let Some(value) = seq.next_element::<StrictValue>()? {
                    values.push(value.0);
                }
                Ok(StrictValue(serde_json::Value::Array(values)))
            }

            fn visit_map<A>(self, mut map: A) -> Result<StrictValue, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut object =
                    serde_json::Map::with_capacity(map.size_hint().unwrap_or(0).min(1024));
                while let Some((key, value)) = map.next_entry::<String, StrictValue>()? {
                    if object.contains_key(&key) {
                        return Err(de::Error::custom("duplicate object key"));
                    }
                    object.insert(key, value.0);
                }
                Ok(StrictValue(serde_json::Value::Object(object)))
            }
        }

        deserializer.deserialize_any(StrictVisitor)
    }
}

/// The one strict JSON decoder shared by collection and status (FR-14,
/// FR-30): serde_json parsing through `StrictValue` so duplicate decoded
/// object member names are rejected at any depth.
fn decode_strict(raw: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<StrictValue>(raw)
        .ok()
        .map(|StrictValue(value)| value)
}

/// Strictly validate one completed assistant record before provider
/// attribution (FR-13-FR-18). Required numeric fields are never
/// defaulted; `as_u64` rejects null, negative, fractional, nonnumeric,
/// and values outside u64. OpenCode's client-local `cost` is ignored.
fn parse_record(db_time_created: i64, raw: &str) -> Option<ParsedRecord> {
    let value = decode_strict(raw)?;
    let object = value.as_object()?;
    if object.get("role")?.as_str()? != "assistant" {
        return None;
    }
    if object.get("error").is_some_and(|error| !error.is_null()) {
        return None;
    }

    let provider_id = object.get("providerID")?.as_str()?.trim();
    let model = object.get("modelID")?.as_str()?.trim();
    if provider_id.is_empty() || model.is_empty() {
        return None;
    }
    if !matches!(
        object.get("finish")?.as_str()?,
        "tool-calls" | "stop" | "length"
    ) {
        return None;
    }

    let time = object.get("time")?.as_object()?;
    let created_ms = i64::try_from(time.get("created")?.as_u64()?).ok()?;
    i64::try_from(time.get("completed")?.as_u64()?).ok()?;
    if created_ms != db_time_created {
        return None;
    }
    let created = DateTime::from_timestamp_millis(created_ms)?;

    let tokens = object.get("tokens")?.as_object()?;
    let input_tokens = tokens.get("input")?.as_u64()?;
    let output = tokens.get("output")?.as_u64()?;
    let reasoning = tokens.get("reasoning")?.as_u64()?;
    let cache = tokens.get("cache")?.as_object()?;
    let cache_read_tokens = cache.get("read")?.as_u64()?;
    let cache_write_tokens = cache.get("write")?.as_u64()?;
    let output_tokens = output.saturating_add(reasoning);
    let total = input_tokens
        .saturating_add(output_tokens)
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_write_tokens);
    if total == 0 {
        return None;
    }

    Some(ParsedRecord {
        provider_id: provider_id.to_string(),
        model: model.to_string(),
        created,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
    })
}

/// Exact, case-sensitive OpenCode provider attribution allowlist (FR-19/20).
/// Whitespace is ignored, but model ids are never inspected or inferred.
fn canonical_provider(provider_id: &str) -> Option<&'static str> {
    match provider_id.trim() {
        "alibaba"
        | "alibaba-cn"
        | "alibaba-coding-plan"
        | "alibaba-coding-plan-cn"
        | "alibaba-token-plan"
        | "alibaba-token-plan-cn"
        | "bailian-token-plan-personal" => Some("qwen"),
        "zai" | "zai-coding-plan" | "zhipuai" | "zhipuai-coding-plan" => Some("glm"),
        "deepseek" => Some("deepseek"),
        "kimi-for-coding" | "moonshot" | "moonshotai" | "kimi" => Some("kimi"),
        "openai" => Some("openai"),
        _ => None,
    }
}

/// Status eligibility: the exact shared record predicate plus the
/// provider allowlist (FR-30). The status scalar calls this; collection
/// uses `parse_record` and classifies a strictly valid unknown provider
/// as unsupported rather than malformed.
fn status_eligible(db_time_created: i64, raw: &str) -> bool {
    match parse_record(db_time_created, raw) {
        Some(record) => canonical_provider(&record.provider_id).is_some(),
        None => false,
    }
}

/// The collector's single query (FR-9/FR-10/FR-11/FR-12): only the
/// `message` table, only `time_created` and `data`, exact `[since,
/// until)` bounds in UTC epoch milliseconds, deterministic order by
/// time then id.
const MESSAGE_QUERY: &str = "SELECT time_created, data FROM message \
     WHERE time_created >= ?1 AND time_created < ?2 \
     ORDER BY time_created, id";

/// Task 4 bounded eligibility probe: one constant-1 single-row read over
/// `message` filtered by the connection-local `strict_eligible` scalar,
/// which is the final eligibility authority (FR-30). SQLite does not
/// independently coerce, path-read, duplicate-check, Unicode-decode, or
/// range-check record JSON; every FR-13 invariant runs in the shared
/// Rust decoder, so status and collection cannot diverge. The query may
/// examine every row when no match exists; no candidate cap or window is
/// applied, because either would create false `no` answers (NFR-2).
const AVAILABLE_QUERY: &str =
    "SELECT 1 FROM message WHERE strict_eligible(time_created, data) = 1 LIMIT 1";

/// Register the probe's one private connection-local scalar (FR-30):
/// UTF8, deterministic, and direct-only. It accepts exactly two
/// arguments — database `time_created` and raw `data` — reads them
/// type-tolerantly through `ValueRef`, and returns the boolean
/// eligibility result. Non-text data, invalid UTF-8, malformed JSON,
/// duplicate keys, strict record rejection, or an unsupported provider
/// all answer `false`; record content never becomes a callback `Err`.
fn register_status_scalar(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "strict_eligible",
        2,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_DIRECTONLY,
        move |ctx| {
            let time_created = match ctx.get_raw(0) {
                ValueRef::Integer(ms) => ms,
                _ => return Ok(false),
            };
            let data = match ctx.get_raw(1) {
                ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(_) => return Ok(false),
                },
                _ => return Ok(false),
            };
            Ok(status_eligible(time_created, data))
        },
    )
}

/// Task 4 eligibility probe over a concrete database path: `yes` iff
/// the metadata safety check passes, the shared READONLY|NOMUTEX +
/// 250 ms + query_only connection succeeds, the private scalar
/// registers, and the bounded SQL finds at least one eligible
/// supported-provider assistant message. Every failure — missing,
/// unreadable, busy, incompatible schema, corrupt, rejected-only —
/// answers `false`; the status never prints DB diagnostics (FR-8).
pub fn available_from(path: &Path) -> bool {
    if metadata_class(path).is_err() {
        return false;
    }
    let conn = match open_readonly(path) {
        Ok(conn) => conn,
        Err(_) => return false,
    };
    if register_status_scalar(&conn).is_err() {
        return false;
    }
    matches!(
        conn.query_row(AVAILABLE_QUERY, [], |row| row.get::<_, i64>(0))
            .optional(),
        Ok(Some(1))
    )
}

/// Task 4 production eligibility probe: resolves the live database
/// through the shared resolver and defers to `available_from`. The
/// answer is a plain boolean — no note, path, or raw error ever
/// reaches `llmu providers` output.
pub fn available() -> bool {
    match std_db_path() {
        Some(path) => available_from(&path),
        None => false,
    }
}

/// Map a rusqlite failure to a bounded category (FR-8). Internal only:
/// the raw error is never emitted. Covers both the open/query variants
/// (`SqliteFailure`) and statement-preparation variants
/// (`SqlInputError`, which carries no meaningful code for missing
/// columns), so schema mismatches classify consistently.
fn classify(err: &rusqlite::Error) -> DbError {
    let (code, msg) = match err {
        rusqlite::Error::SqliteFailure(ffi_err, msg) => (Some(ffi_err.code), msg.as_deref()),
        rusqlite::Error::SqlInputError { error, msg, .. } => (Some(error.code), Some(msg.as_str())),
        _ => (None, None),
    };
    match code {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            DbError::Busy
        }
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
    let mut rows = stmt
        .query(rusqlite::params![since_ms, until_ms])
        .map_err(|e| classify(&e))?;
    while let Some(row) = rows.next().map_err(|e| classify(&e))? {
        let data = match row.get_ref(1).map_err(|e| classify(&e))? {
            ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => Some(text.to_string()),
                Err(_) => None,
            },
            _ => None,
        };
        on_row(MessageRow {
            time_created_ms: row.get::<_, i64>(0).map_err(|e| classify(&e))?,
            data,
        });
    }
    Ok(())
}

const NOTE_UNREADABLE: &str = "opencode: local usage database is busy or unreadable";
const NOTE_INCOMPATIBLE: &str = "opencode: local usage database schema is unsupported";

/// Collect from a concrete database path (FR-3/FR-25). Hermetic core:
/// the path is injected so tests run against synthetic databases only
/// (NFR-8); the std entry point resolves the live path. Events stay
/// staged until the row iterator completes, so a database-level
/// failure cannot leak partial events or partial row counts (FR-34).
pub fn collect_from(
    cfg: &Config,
    path: &Path,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Collected {
    match metadata_class(path) {
        Ok(()) => {}
        Err(DbError::Missing) => {
            return Collected {
                events: vec![],
                notes: vec![],
            };
        }
        Err(_) => {
            return database_failure(NOTE_UNREADABLE);
        }
    }
    let conn = match open_readonly(path) {
        Ok(conn) => conn,
        Err(_) => return database_failure(NOTE_UNREADABLE),
    };
    let since_ms = since.timestamp_millis();
    let until_ms = until.timestamp_millis();

    // (hour, provider, model) -> [requests, input, output, cache read, cache write]
    let mut aggregate: BTreeMap<(DateTime<Utc>, String, String), [u64; 5]> = BTreeMap::new();
    let mut malformed = 0u64;
    let mut unsupported = 0u64;
    let streamed = stream_message_rows(&conn, since_ms, until_ms, |row| {
        let Some(data) = row.data else {
            malformed = malformed.saturating_add(1);
            return;
        };
        let Some(record) = parse_record(row.time_created_ms, &data) else {
            malformed = malformed.saturating_add(1);
            return;
        };
        let Some(provider) = canonical_provider(&record.provider_id) else {
            unsupported = unsupported.saturating_add(1);
            return;
        };
        let hour = record
            .created
            .with_minute(0)
            .and_then(|time| time.with_second(0))
            .and_then(|time| time.with_nanosecond(0))
            .expect("valid UTC timestamp can be truncated to an hour");
        let entry = aggregate
            .entry((hour, provider.to_string(), record.model))
            .or_default();
        entry[0] = entry[0].saturating_add(1);
        entry[1] = entry[1].saturating_add(record.input_tokens);
        entry[2] = entry[2].saturating_add(record.output_tokens);
        entry[3] = entry[3].saturating_add(record.cache_read_tokens);
        entry[4] = entry[4].saturating_add(record.cache_write_tokens);
    });
    if let Err(error) = streamed {
        return match error {
            DbError::Incompatible => database_failure(NOTE_INCOMPATIBLE),
            _ => database_failure(NOTE_UNREADABLE),
        };
    }

    let events = aggregate
        .into_iter()
        .map(|((start, provider, model), values)| UsageEvent {
            cost_usd: cfg.estimate_cost(&model, values[1], values[2], values[3], values[4]),
            provider,
            source: SourceKind::LocalLogs,
            model,
            start,
            requests: values[0],
            input_tokens: values[1],
            output_tokens: values[2],
            cache_read_tokens: values[3],
            cache_write_tokens: values[4],
            tool_calls: 0,
            cost_is_estimate: true,
        })
        .collect();
    let mut notes = vec![];
    if malformed > 0 {
        notes.push(format!(
            "opencode: skipped {malformed} malformed local usage record(s)"
        ));
    }
    if unsupported > 0 {
        notes.push(format!(
            "opencode: skipped {unsupported} local usage record(s) from unsupported provider(s)"
        ));
    }
    Collected { events, notes }
}

fn database_failure(note: &str) -> Collected {
    Collected {
        events: vec![],
        notes: vec![note.to_string()],
    }
}

/// Std entry point (FR-3): resolves the live database through the
/// shared resolver and defers to `collect_from`.
#[allow(dead_code)] // not yet reachable; gather/CLI wiring lands in Task 5
pub fn collect(cfg: &Config, since: DateTime<Utc>, until: DateTime<Utc>) -> Collected {
    match std_db_path() {
        Some(path) => collect_from(cfg, &path, since, until),
        None => Collected {
            events: vec![],
            notes: vec![],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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

    fn config_with_price(model: &str, price: [f64; 4]) -> Config {
        let mut cfg = Config::default();
        cfg.pricing.insert(model.to_string(), price);
        cfg
    }

    fn valid_data(provider: &str, model: &str, created: i64) -> serde_json::Value {
        serde_json::json!({
            "role": "assistant",
            "providerID": provider,
            "modelID": model,
            "time": {"created": created, "completed": created + 1},
            "finish": "stop",
            "tokens": {
                "input": 10,
                "output": 20,
                "reasoning": 5,
                "cache": {"read": 3, "write": 2}
            }
        })
    }

    fn write_values(dir: &Path, rows: &[(i64, &str, serde_json::Value)]) -> PathBuf {
        let owned: Vec<(i64, &str, String)> = rows
            .iter()
            .map(|(time, id, data)| (*time, *id, data.to_string()))
            .collect();
        let borrowed: Vec<(i64, &str, &str)> = owned
            .iter()
            .map(|(time, id, data)| (*time, *id, data.as_str()))
            .collect();
        write_db(dir, &borrowed)
    }

    fn no_env(_k: &str) -> Option<String> {
        None
    }

    fn env_pairs<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
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
            db_path(
                &env_pairs(&[("XDG_DATA_HOME", "/tmp/llmu-xdg")]),
                Some(home)
            ),
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
        let out = collect_from(
            &Config::default(),
            &dir.join("opencode.db"),
            ms(0),
            ms(1000),
        );
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
        let out = collect_from(
            &Config::default(),
            &blocker.join("opencode.db"),
            ms(0),
            ms(1000),
        );
        assert!(out.events.is_empty());
        assert_eq!(
            out.notes.len(),
            1,
            "a non-NotFound metadata failure is categorized (FR-3)"
        );
        assert_eq!(
            out.notes[0],
            "opencode: local usage database is busy or unreadable"
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
        let out = collect_from(&Config::default(), &path, ms(0), ms(1000));
        assert!(out.events.is_empty());
        assert_eq!(out.notes.len(), 1);
        assert_eq!(
            out.notes[0],
            "opencode: local usage database is busy or unreadable"
        );
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
        assert_eq!(got[0].data.as_deref(), Some(r#"{"text":"two"}"#));
        assert_eq!(got[1].time_created_ms, 300);
        assert_eq!(got[1].data.as_deref(), Some(r#"{"text":"three"}"#));
        let mut none = vec![];
        stream_message_rows(&conn, 400, 400, |r| none.push(r)).unwrap();
        assert!(
            none.is_empty(),
            "an empty half-open window yields no rows (FR-10)"
        );
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
            got[1].data.as_deref(),
            Some(r#"{"x":3}"#),
            "same-time rows order by id (row-a before row-b) (FR-10)"
        );
        assert_eq!(got[2].data.as_deref(), Some(r#"{"x":1}"#));
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
        let path = write_values(
            &temp_dir("roundtrip"),
            &[(100, "m1", valid_data("openai", "model-a", 100))],
        );
        let out = collect_from(&Config::default(), &path, ms(0), ms(1000));
        assert!(
            !out.events.is_empty(),
            "Task 3 normalizes eligible records (FR-13-FR-18)"
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
        let out = collect_from(&Config::default(), &empty_db, ms(0), ms(1000));
        assert!(out.events.is_empty());
        assert_eq!(out.notes.len(), 1);
        assert_eq!(
            out.notes[0],
            "opencode: local usage database schema is unsupported"
        );
        assert!(
            !out.notes[0].contains("opencode.db"),
            "notes never carry paths (FR-8): {:?}",
            out.notes[0]
        );
    }

    #[test]
    fn collect_never_touches_db_bytes_hash_or_mtime() {
        let path = write_values(
            &temp_dir("nfr5"),
            &[(100, "m1", valid_data("openai", "model-a", 100))],
        );
        let before = snapshot(&path);
        let out = collect_from(&Config::default(), &path, ms(0), ms(1000));
        assert_eq!(out.events.len(), 1);
        assert!(out.notes.is_empty());
        let after = snapshot(&path);
        assert_eq!(
            before, after,
            "read-only collect leaves length, hash, and mtime unchanged (NFR-5)"
        );
    }

    #[test]
    fn parser_accepts_finish_states_and_normalizes_tokens_without_cache_subtraction() {
        for finish in ["tool-calls", "stop", "length"] {
            let mut data = valid_data("openai", " model-a ", 1_000);
            data["finish"] = serde_json::json!(finish);
            data["error"] = serde_json::Value::Null;
            data["cost"] = serde_json::json!(999_999.0);
            let parsed = parse_record(1_000, &data.to_string()).expect("eligible record");
            assert_eq!(parsed.provider_id, "openai");
            assert_eq!(parsed.model, "model-a");
            assert_eq!(parsed.input_tokens, 10, "cache is not subtracted");
            assert_eq!(parsed.output_tokens, 25, "reasoning folds into output");
            assert_eq!(parsed.cache_read_tokens, 3);
            assert_eq!(parsed.cache_write_tokens, 2);
        }
    }

    #[test]
    fn parser_rejects_every_required_field_failure() {
        let base = valid_data("openai", "model-a", 1_000);
        let mut invalid = Vec::new();
        invalid.push(serde_json::json!([]));
        invalid.push({
            let mut v = base.clone();
            v["role"] = serde_json::json!("user");
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["role"] = serde_json::json!("system");
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["error"] = serde_json::json!({"name":"safe-test-error"});
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["finish"] = serde_json::json!("cancelled");
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["providerID"] = serde_json::json!("  ");
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["modelID"] = serde_json::Value::Null;
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["modelID"] = serde_json::json!("  ");
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["time"]["created"] = serde_json::json!(-1);
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["time"]["completed"] = serde_json::json!(1.5);
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["time"]["completed"] = serde_json::json!(-1);
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["time"].as_object_mut().unwrap().remove("completed");
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["tokens"]["input"] = serde_json::json!(-1);
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["tokens"] = serde_json::json!([]);
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["tokens"]["output"] = serde_json::json!(1.5);
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["tokens"]["reasoning"] = serde_json::Value::Null;
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["tokens"]["cache"]["read"] = serde_json::json!({});
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["tokens"]["cache"] = serde_json::Value::Null;
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["tokens"]["cache"]
                .as_object_mut()
                .unwrap()
                .remove("write");
            v
        });
        invalid.push({
            let mut v = base.clone();
            v["tokens"]["input"] = serde_json::json!(0);
            v["tokens"]["output"] = serde_json::json!(0);
            v["tokens"]["reasoning"] = serde_json::json!(0);
            v["tokens"]["cache"]["read"] = serde_json::json!(0);
            v["tokens"]["cache"]["write"] = serde_json::json!(0);
            v
        });

        assert!(parse_record(1_000, "not-json").is_none());
        for (index, value) in invalid.into_iter().enumerate() {
            assert!(
                parse_record(1_000, &value.to_string()).is_none(),
                "invalid required-field case {index} was accepted: {value}"
            );
        }
        assert!(
            parse_record(999, &base.to_string()).is_none(),
            "database/data created timestamp mismatch rejects the row"
        );
        let too_large = r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":18446744073709551616,"completed":1},"finish":"stop","tokens":{"input":1,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}"#;
        assert!(parse_record(1, too_large).is_none());
        let invalid_timestamp = format!(
            r#"{{"role":"assistant","providerID":"openai","modelID":"m","time":{{"created":{},"completed":{}}},"finish":"stop","tokens":{{"input":1,"output":0,"reasoning":0,"cache":{{"read":0,"write":0}}}}}}"#,
            i64::MAX,
            i64::MAX
        );
        assert!(parse_record(i64::MAX, &invalid_timestamp).is_none());
        let token_overflow = r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1,"completed":2},"finish":"stop","tokens":{"input":18446744073709551616,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}"#;
        assert!(parse_record(1, token_overflow).is_none());
    }

    #[test]
    fn provider_allowlist_is_exact_trimmed_and_case_sensitive() {
        for (expected, ids) in [
            (
                "qwen",
                &[
                    "alibaba",
                    "alibaba-cn",
                    "alibaba-coding-plan",
                    "alibaba-coding-plan-cn",
                    "alibaba-token-plan",
                    "alibaba-token-plan-cn",
                    "bailian-token-plan-personal",
                ][..],
            ),
            (
                "glm",
                &["zai", "zai-coding-plan", "zhipuai", "zhipuai-coding-plan"][..],
            ),
            ("deepseek", &["deepseek"][..]),
            (
                "kimi",
                &["kimi-for-coding", "moonshot", "moonshotai", "kimi"][..],
            ),
            ("openai", &["openai"][..]),
        ] {
            for id in ids {
                assert_eq!(canonical_provider(id), Some(expected), "alias {id}");
                assert_eq!(canonical_provider(&format!(" {id} ")), Some(expected));
            }
        }
        for id in ["opencode", "OpenAI", "QWEN", "unknown", ""] {
            assert_eq!(canonical_provider(id), None, "unsupported id {id}");
        }
    }

    #[test]
    fn collector_aggregates_hourly_prices_normalized_buckets_and_exact_bounds() {
        let hour = 3_600_000;
        let mut first = valid_data("openai", "priced-model", hour);
        first["cost"] = serde_json::json!(42_000.0);
        let second = valid_data("openai", "priced-model", hour + 3_599_999);
        let excluded = valid_data("openai", "priced-model", hour + 3_600_000);
        let path = write_values(
            &temp_dir("aggregate"),
            &[
                (hour, "a", first),
                (hour + 3_599_999, "b", second),
                (hour + 3_600_000, "c", excluded),
            ],
        );
        let cfg = config_with_price("priced-model", [1.0, 2.0, 3.0, 4.0]);
        let out = collect_from(&cfg, &path, ms(hour), ms(hour + 3_600_000));
        assert!(out.notes.is_empty(), "notes: {:?}", out.notes);
        assert_eq!(out.events.len(), 1);
        let event = &out.events[0];
        assert_eq!(event.provider, "openai");
        assert_eq!(event.model, "priced-model");
        assert_eq!(event.start, ms(hour));
        assert_eq!(event.requests, 2);
        assert_eq!(event.input_tokens, 20);
        assert_eq!(event.output_tokens, 50);
        assert_eq!(event.cache_read_tokens, 6);
        assert_eq!(event.cache_write_tokens, 4);
        assert_eq!(event.tool_calls, 0);
        assert!(event.cost_is_estimate);
        let expected = (20.0 + 100.0 + 18.0 + 16.0) / 1_000_000.0;
        assert_eq!(event.cost_usd, Some(expected), "OpenCode cost is ignored");
    }

    #[test]
    fn collector_separates_keys_orders_deterministically_and_saturates() {
        let mut saturated = valid_data("deepseek", "same-model", 3_600_000);
        saturated["tokens"]["input"] = serde_json::json!(u64::MAX);
        saturated["tokens"]["output"] = serde_json::json!(u64::MAX);
        saturated["tokens"]["reasoning"] = serde_json::json!(1);
        let mut saturated_next = saturated.clone();
        saturated_next["time"]["created"] = serde_json::json!(3_600_001);
        saturated_next["time"]["completed"] = serde_json::json!(3_600_002);
        let path = write_values(
            &temp_dir("separate"),
            &[
                (7_200_000, "z", valid_data("openai", "z-model", 7_200_000)),
                (3_600_000, "b", saturated.clone()),
                (3_600_001, "c", saturated_next),
                (3_600_002, "a", valid_data("zai", "a-model", 3_600_002)),
            ],
        );
        let out = collect_from(&Config::default(), &path, ms(0), ms(10_800_000));
        assert_eq!(out.events.len(), 3);
        assert_eq!(out.events[0].provider, "deepseek");
        assert_eq!(out.events[0].requests, 2);
        assert_eq!(out.events[0].input_tokens, u64::MAX);
        assert_eq!(out.events[0].output_tokens, u64::MAX);
        assert_eq!(
            out.events[0].cost_usd, None,
            "an unknown pricing key produces no estimate"
        );
        assert_eq!(out.events[1].provider, "glm");
        assert_eq!(out.events[2].provider, "openai");
        assert_eq!(out.events[2].start, ms(7_200_000));
    }

    #[test]
    fn collector_counts_malformed_and_unsupported_once_without_leaking_values() {
        let mut malformed_unknown = valid_data("private-unknown", "secret-model", 100);
        malformed_unknown["finish"] = serde_json::json!("cancelled");
        let path = write_values(
            &temp_dir("notes"),
            &[
                (100, "private-id", malformed_unknown),
                (
                    200,
                    "unknown-id",
                    valid_data("private-unknown", "secret-model", 200),
                ),
                (
                    300,
                    "generic-id",
                    valid_data("opencode", "secret-generic", 300),
                ),
            ],
        );
        let out = collect_from(&Config::default(), &path, ms(0), ms(1_000));
        assert!(out.events.is_empty());
        assert_eq!(
            out.notes,
            vec![
                "opencode: skipped 1 malformed local usage record(s)",
                "opencode: skipped 2 local usage record(s) from unsupported provider(s)",
            ]
        );
        let notes = out.notes.join("\n");
        for secret in [
            "private-unknown",
            "secret-model",
            "secret-generic",
            "private-id",
        ] {
            assert!(!notes.contains(secret));
        }
    }

    #[test]
    fn non_text_and_invalid_utf8_data_is_malformed_not_database_failure() {
        let dir = temp_dir("nontext");
        let path = dir.join("opencode.db");
        let w = Connection::open(&path).unwrap();
        w.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, sessionID TEXT, time_created INTEGER, \
             time_updated INTEGER, role TEXT, providerID TEXT, modelID TEXT, data TEXT)",
        )
        .unwrap();
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "a-valid",
                100_i64,
                valid_data("openai", "model-a", 100).to_string()
            ],
        )
        .unwrap();
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["z-blob", 200_i64, vec![0xff_u8]],
        )
        .unwrap();
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["z-int", 300_i64, 42_i64],
        )
        .unwrap();
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["z-real", 400_i64, 4.2_f64],
        )
        .unwrap();
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, CAST(?3 AS TEXT))",
            rusqlite::params!["z-utf8", 500_i64, vec![0xff_u8]],
        )
        .unwrap();
        w.execute(
            "INSERT INTO message (id, time_created) VALUES (?1, ?2)",
            rusqlite::params!["z-null", 600_i64],
        )
        .unwrap();
        drop(w);
        let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
        assert_eq!(
            out.events.len(),
            1,
            "the valid sibling row must still collect (FR-34)"
        );
        assert_eq!(
            out.notes,
            vec!["opencode: skipped 5 malformed local usage record(s)"],
            "non-text and invalid-UTF-8 data is malformed, never a database failure (FR-14)"
        );
        assert!(
            available_from(&path),
            "the eligible sibling still answers yes"
        );
        let before = snapshot(&path);
        assert!(available_from(&path));
        let after = snapshot(&path);
        assert_eq!(
            before, after,
            "the probe never writes: length, hash, and mtime stay untouched (NFR-5)"
        );
    }

    #[test]
    fn genuine_row_failure_discards_staged_events_atomically() {
        let dir = temp_dir("genuine-atomic");
        let path = dir.join("opencode.db");
        let w = Connection::open(&path).unwrap();
        w.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, sessionID TEXT, time_created INTEGER, \
             time_updated INTEGER, role TEXT, providerID TEXT, modelID TEXT, data TEXT)",
        )
        .unwrap();
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "a-valid",
                100_i64,
                valid_data("openai", "model-a", 100).to_string()
            ],
        )
        .unwrap();
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "z-real-time",
                9_999.5_f64,
                valid_data("openai", "model-b", 200).to_string()
            ],
        )
        .unwrap();
        w.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "b-valid",
                300_i64,
                valid_data("openai", "model-c", 300).to_string()
            ],
        )
        .unwrap();
        drop(w);
        let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
        assert!(
            out.events.is_empty(),
            "a genuine row failure after staged rows must discard everything (FR-34)"
        );
        assert_eq!(
            out.notes,
            vec!["opencode: local usage database is busy or unreadable"]
        );
    }

    #[test]
    fn collector_busy_failure_is_atomic_and_uses_exact_bounded_note() {
        let path = write_values(
            &temp_dir("collect-busy"),
            &[(100, "a-valid", valid_data("openai", "model-a", 100))],
        );
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let out = collect_from(&Config::default(), &path, ms(0), ms(1_000));
        assert!(out.events.is_empty());
        assert_eq!(
            out.notes,
            vec!["opencode: local usage database is busy or unreadable"]
        );
    }

    // -------------------------------------------------------------------
    // Task 4: bounded availability probe.
    // -------------------------------------------------------------------

    #[test]
    fn available_missing_unreadable_and_incompatible_all_false() {
        let dir = temp_dir("avail-missing");
        assert!(
            !available_from(&dir.join("opencode.db")),
            "a missing database answers no (FR-3)"
        );
        let garbage = dir.join("garbage.db");
        std::fs::write(&garbage, "definitely not a sqlite database").unwrap();
        assert!(!available_from(&garbage), "a corrupt file answers no");
        let empty = dir.join("empty.db");
        Connection::open(&empty).unwrap();
        assert!(
            !available_from(&empty),
            "a database without the message table answers no"
        );
    }

    #[test]
    fn available_eligible_db_yes_and_rejected_only_db_no() {
        let eligible = write_values(
            &temp_dir("avail-elig"),
            &[(1_000, "m1", valid_data("openai", "model-a", 1_000))],
        );
        assert!(
            available_from(&eligible),
            "at least one eligible record answers yes"
        );
        let before = snapshot(&eligible);
        assert!(available_from(&eligible));
        let after = snapshot(&eligible);
        assert_eq!(
            before, after,
            "the probe never writes: length, hash, and mtime stay untouched (NFR-5)"
        );

        let unknown = write_values(
            &temp_dir("avail-rejected"),
            &[(1_000, "m1", valid_data("private-unknown", "model-a", 1_000))],
        );
        assert!(
            !available_from(&unknown),
            "records from unsupported providers answer no"
        );
        let generic = write_values(
            &temp_dir("avail-generic"),
            &[(1_000, "m1", valid_data("opencode", "model-a", 1_000))],
        );
        assert!(
            !available_from(&generic),
            "the generic `opencode` provider id answers no"
        );
    }

    #[test]
    fn available_finds_any_eligible_record_without_window_bound() {
        let path = write_values(
            &temp_dir("avail-window"),
            &[(0, "ancient", valid_data("deepseek", "model-a", 0))],
        );
        assert!(
            available_from(&path),
            "eligibility is window-free: an ancient eligible record still answers yes"
        );
    }

    #[test]
    fn available_parity_accepted_finishes_and_aliases_also_collect() {
        let aliases = [
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
        let mut case = 0;
        for alias in aliases {
            for finish in ["tool-calls", "stop", "length"] {
                let mut data = valid_data(alias, "model-a", 1_000);
                data["finish"] = serde_json::json!(finish);
                data["error"] = serde_json::Value::Null;
                let path = write_values(
                    &temp_dir(&format!("avail-accept-{case}")),
                    &[(1_000, &format!("m-{case}"), data)],
                );
                assert!(
                    available_from(&path),
                    "alias {alias} with finish {finish} must answer yes"
                );
                let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
                assert_eq!(
                    out.events.len(),
                    1,
                    "alias {alias} with finish {finish} must collect an event"
                );
                assert!(out.notes.is_empty(), "notes: {:?}", out.notes);
                case += 1;
            }
        }
    }

    #[test]
    fn available_parity_rejected_dimensions_collect_nothing() {
        let base = valid_data("openai", "model-a", 1_000);
        let mut invalid = Vec::new();
        invalid.push(serde_json::json!([{ "role": "assistant" }]));
        for role in ["user", "system"] {
            let mut v = base.clone();
            v["role"] = serde_json::json!(role);
            invalid.push(v);
        }
        let mut v = base.clone();
        v["error"] = serde_json::json!({"name": "safe-test-error"});
        invalid.push(v);
        let mut v = base.clone();
        v["finish"] = serde_json::json!("cancelled");
        invalid.push(v);
        for provider in ["  ", "OpenAI", "opencode", "private-unknown"] {
            let mut v = base.clone();
            v["providerID"] = serde_json::json!(provider);
            invalid.push(v);
        }
        let mut v = base.clone();
        v["providerID"] = serde_json::json!(42);
        invalid.push(v);
        for model in [
            serde_json::Value::Null,
            serde_json::json!("  "),
            serde_json::json!(7),
        ] {
            let mut v = base.clone();
            v["modelID"] = model;
            invalid.push(v);
        }
        for created in [serde_json::json!(-1), serde_json::json!(1.5)] {
            let mut v = base.clone();
            v["time"]["created"] = created;
            invalid.push(v);
        }
        for completed in [serde_json::json!(-1), serde_json::json!(1.5)] {
            let mut v = base.clone();
            v["time"]["completed"] = completed;
            invalid.push(v);
        }
        let mut v = base.clone();
        v["time"].as_object_mut().unwrap().remove("completed");
        invalid.push(v);
        let mut v = base.clone();
        v["time"].as_object_mut().unwrap().remove("created");
        invalid.push(v);
        let mut v = base.clone();
        v["time"]["created"] = serde_json::json!(999);
        invalid.push(v); // the DB row time_created stays 1000 -> mismatch
        for input in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::Value::Null,
        ] {
            let mut v = base.clone();
            v["tokens"]["input"] = input;
            invalid.push(v);
        }
        for output in [serde_json::json!(-1), serde_json::json!(1.5)] {
            let mut v = base.clone();
            v["tokens"]["output"] = output;
            invalid.push(v);
        }
        let mut v = base.clone();
        v["tokens"]["reasoning"] = serde_json::Value::Null;
        invalid.push(v);
        let mut v = base.clone();
        v["tokens"]["cache"]["read"] = serde_json::json!({});
        invalid.push(v);
        let mut v = base.clone();
        v["tokens"]["cache"]["write"] = serde_json::json!(-1);
        invalid.push(v);
        let mut v = base.clone();
        v["tokens"]["cache"] = serde_json::Value::Null;
        invalid.push(v);
        let mut v = base.clone();
        v["tokens"]["cache"].as_object_mut().unwrap().remove("read");
        invalid.push(v);
        let mut v = base.clone();
        v["tokens"] = serde_json::json!([]);
        invalid.push(v);
        let mut v = base.clone();
        v["tokens"]["input"] = serde_json::json!(0);
        v["tokens"]["output"] = serde_json::json!(0);
        v["tokens"]["reasoning"] = serde_json::json!(0);
        v["tokens"]["cache"]["read"] = serde_json::json!(0);
        v["tokens"]["cache"]["write"] = serde_json::json!(0);
        invalid.push(v);

        for (index, data) in invalid.into_iter().enumerate() {
            let path = write_values(
                &temp_dir(&format!("avail-reject-{index}")),
                &[(1_000, "m", data)],
            );
            assert!(
                !available_from(&path),
                "rejected case {index} must answer no"
            );
            let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
            assert_eq!(
                out.events.len(),
                0,
                "rejected case {index} must collect no events"
            );
        }

        let malformed = write_db(
            &temp_dir("avail-reject-malformed"),
            &[(1_000, "m", "{not json")],
        );
        assert!(!available_from(&malformed), "malformed JSON answers no");
        let out = collect_from(&Config::default(), &malformed, ms(0), ms(10_000));
        assert_eq!(out.events.len(), 0);
    }

    #[test]
    fn available_parity_overflowing_json_integer_literals_answer_no() {
        for (index, literal) in ["9223372036854775808", "18446744073709551616"]
            .iter()
            .enumerate()
        {
            let row = format!(
                r#"{{"role":"assistant","providerID":"openai","modelID":"m","time":{{"created":{literal},"completed":1}},"finish":"stop","tokens":{{"input":1,"output":0,"reasoning":0,"cache":{{"read":0,"write":0}}}}}}"#
            );
            let path = write_db(
                &temp_dir(&format!("avail-overflow-time-{index}")),
                &[(1, "m", &row)],
            );
            assert!(
                !available_from(&path),
                "created literal {literal} must answer no"
            );
            let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
            assert_eq!(
                out.events.len(),
                0,
                "created literal {literal} must collect no events"
            );
        }
        let row = r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1,"completed":2},"finish":"stop","tokens":{"input":18446744073709551616,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}"#;
        let path = write_db(&temp_dir("avail-overflow-token"), &[(1, "m", row)]);
        assert!(
            !available_from(&path),
            "an input token literal past u64 must answer no"
        );
        let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
        assert_eq!(out.events.len(), 0);
    }

    #[test]
    fn parser_accepts_token_buckets_at_2p63_and_u64max() {
        for bucket in [9_223_372_036_854_775_808u64, u64::MAX] {
            for field in ["input", "output", "reasoning"] {
                let mut data = valid_data("openai", "model-a", 1_000);
                data["tokens"][field] = serde_json::json!(bucket);
                let parsed = parse_record(1_000, &data.to_string())
                    .unwrap_or_else(|| panic!("{field} at {bucket} must parse"));
                assert!(
                    parsed.input_tokens > 0 || parsed.output_tokens > 0,
                    "{field} at {bucket} must produce a positive total"
                );
            }
            for field in ["read", "write"] {
                let mut data = valid_data("openai", "model-a", 1_000);
                data["tokens"]["cache"][field] = serde_json::json!(bucket);
                let parsed = parse_record(1_000, &data.to_string())
                    .unwrap_or_else(|| panic!("cache.{field} at {bucket} must parse"));
                assert!(
                    parsed.cache_read_tokens > 0 || parsed.cache_write_tokens > 0,
                    "cache.{field} at {bucket} must produce a positive total"
                );
            }
        }
    }

    #[test]
    fn available_parity_token_buckets_u64_range_yes_and_2p64_no() {
        for (field, cache) in [
            ("input", false),
            ("output", false),
            ("reasoning", false),
            ("read", true),
            ("write", true),
        ] {
            for bucket in [9_223_372_036_854_775_808u64, u64::MAX] {
                let mut data = valid_data("openai", "model-a", 1_000);
                if cache {
                    data["tokens"]["cache"][field] = serde_json::json!(bucket);
                } else {
                    data["tokens"][field] = serde_json::json!(bucket);
                }
                let path = write_values(
                    &temp_dir(&format!("avail-tok-{field}-{bucket}")),
                    &[(1_000, "m", data)],
                );
                assert!(
                    available_from(&path),
                    "tokens.{field} at {bucket} must answer yes"
                );
                let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
                assert_eq!(
                    out.events.len(),
                    1,
                    "tokens.{field} at {bucket} must collect an event"
                );
            }
        }
        let base = r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#;
        for (field, literal) in [
            ("input", r#""input":10"#),
            ("output", r#""output":20"#),
            ("reasoning", r#""reasoning":5"#),
            ("read", r#""read":3"#),
            ("write", r#""write":2"#),
        ] {
            let row = base.replace(literal, &format!("\"{field}\":18446744073709551616"));
            let path = write_db(
                &temp_dir(&format!("avail-2p64-{field}")),
                &[(1_000, "m", &row)],
            );
            assert!(
                !available_from(&path),
                "tokens.{field} literal 18446744073709551616 must answer no"
            );
            let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
            assert_eq!(
                out.events.len(),
                0,
                "tokens.{field} literal 18446744073709551616 must collect nothing"
            );
        }
    }

    #[test]
    fn available_parity_created_upper_bound_matches_locked_chrono() {
        let created_max = 8_210_266_876_799_999i64;
        assert!(
            DateTime::<Utc>::from_timestamp_millis(created_max).is_some(),
            "the locked Chrono must accept created_max"
        );
        assert!(
            DateTime::<Utc>::from_timestamp_millis(created_max + 1).is_none(),
            "the locked Chrono must reject created_max + 1"
        );
        assert!(
            DateTime::<Utc>::from_timestamp_millis(i64::MAX).is_none(),
            "the locked Chrono must reject i64::MAX milliseconds"
        );
        for (created, expect) in [
            (created_max, true),
            (created_max + 1, false),
            (i64::MAX, false),
        ] {
            let mut data = valid_data("openai", "model-a", 1_000);
            data["time"]["created"] = serde_json::json!(created);
            data["time"]["completed"] = serde_json::json!(created);
            let row = data.to_string();
            assert_eq!(
                parse_record(created, &row).is_some(),
                expect,
                "parser must agree on created {created}"
            );
            let path = write_db(
                &temp_dir(&format!("avail-created-{created}")),
                &[(created, "m", &row)],
            );
            assert_eq!(
                available_from(&path),
                expect,
                "probe must agree on created {created}"
            );
            let conn = open_readonly(&path).unwrap();
            let mut rows = vec![];
            stream_message_rows(&conn, 0, created.saturating_add(1), |r| rows.push(r)).unwrap();
            assert_eq!(
                rows.iter()
                    .filter(|r| r.data.as_deref().is_some_and(|d| parse_record(
                        r.time_created_ms,
                        d
                    )
                    .is_some()))
                    .count(),
                usize::from(expect),
                "the collector must agree on created {created}"
            );
        }
        let mut near = valid_data("openai", "model-a", created_max - 60_000);
        near["time"]["created"] = serde_json::json!(created_max - 60_000);
        near["time"]["completed"] = serde_json::json!(created_max - 60_000);
        let path = write_values(
            &temp_dir("avail-created-near-max"),
            &[(created_max - 60_000, "m", near)],
        );
        let out = collect_from(&Config::default(), &path, ms(0), ms(created_max));
        assert_eq!(
            out.events.len(),
            1,
            "collect must accept large created values"
        );
    }

    #[test]
    fn available_parity_whitespace_trim_matches_rust_trim() {
        for provider in [
            "\nopenai\t",
            " \topenai\r\n",
            "\u{3000}openai\u{85}",
            "\u{2003}openai\u{2028}",
        ] {
            let mut data = valid_data(provider, "model-a", 1_000);
            data["cost"] = serde_json::Value::Null;
            let path = write_values(
                &temp_dir(&format!("avail-ws-p-{}", provider.escape_default())),
                &[(1_000, "m", data)],
            );
            assert!(
                available_from(&path),
                "provider {provider:?} must trim to an allowlisted id"
            );
            let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
            assert_eq!(out.events.len(), 1, "provider {provider:?}");
            assert_eq!(out.events[0].provider, "openai");
        }
        for provider in ["\t \n", " \u{3000}\u{2003} ", "\r\n\t"] {
            let data = valid_data(provider, "model-a", 1_000);
            let path = write_values(
                &temp_dir(&format!("avail-ws-pempty-{}", provider.escape_default())),
                &[(1_000, "m", data)],
            );
            assert!(
                !available_from(&path),
                "whitespace-only provider {provider:?} must answer no"
            );
            let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
            assert_eq!(out.events.len(), 0, "whitespace-only provider {provider:?}");
        }
        for model in ["\nmodel-a\t", "\u{2003}model-a\u{2028}"] {
            let mut data = valid_data("openai", model, 1_000);
            data["cost"] = serde_json::Value::Null;
            let path = write_values(
                &temp_dir(&format!("avail-ws-m-{}", model.escape_default())),
                &[(1_000, "m", data)],
            );
            assert!(
                available_from(&path),
                "model {model:?} must trim to a nonempty id"
            );
            let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
            assert_eq!(out.events.len(), 1, "model {model:?}");
            assert_eq!(out.events[0].model, "model-a");
        }
        for model in ["\t\n", "\u{3000} "] {
            let data = valid_data("openai", model, 1_000);
            let path = write_values(
                &temp_dir(&format!("avail-ws-mempty-{}", model.escape_default())),
                &[(1_000, "m", data)],
            );
            assert!(
                !available_from(&path),
                "whitespace-only model {model:?} must answer no"
            );
            let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
            assert_eq!(out.events.len(), 0, "whitespace-only model {model:?}");
        }
    }

    #[test]
    fn available_parity_lexical_negative_zero_rejected() {
        let base = r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#;
        for (needle, replacement, time_created) in [
            (r#""created":1000"#, r#""created":-0"#, 0),
            (r#""completed":1001"#, r#""completed":-0"#, 1_000),
            (r#""input":10"#, r#""input":-0"#, 1_000),
            (r#""output":20"#, r#""output":-0"#, 1_000),
            (r#""reasoning":5"#, r#""reasoning":-0"#, 1_000),
            (r#""read":3"#, r#""read":-0"#, 1_000),
            (r#""write":2"#, r#""write":-0"#, 1_000),
        ] {
            let row = base.replace(needle, replacement);
            assert!(
                parse_record(time_created, &row).is_none(),
                "the parser must reject {needle} as {replacement}"
            );
            let path = write_db(
                &temp_dir(&format!("avail-negzero-{needle}")),
                &[(time_created, "m", &row)],
            );
            assert!(
                !available_from(&path),
                "the probe must reject {needle} as {replacement}"
            );
            let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
            assert_eq!(
                out.events.len(),
                0,
                "{needle} as {replacement} must collect nothing"
            );
        }
    }

    /// Bidirectional status/collector parity for one raw JSON record:
    /// the strict decoder, the probe, and the collector over a covering
    /// window must all agree on eligibility (FR-13/14, FR-30).
    fn assert_record_parity(index: usize, time_created: i64, raw: &str, expect: bool) {
        assert_eq!(
            parse_record(time_created, raw).is_some(),
            expect,
            "case {index}: parser parity for {raw}"
        );
        let path = write_db(
            &temp_dir(&format!("parity-{index}")),
            &[(time_created, "m", raw)],
        );
        assert_eq!(
            available_from(&path),
            expect,
            "case {index}: probe parity for {raw}"
        );
        let out = collect_from(&Config::default(), &path, ms(0), ms(time_created + 60_000));
        assert_eq!(
            out.events.len(),
            usize::from(expect),
            "case {index}: collector parity for {raw}"
        );
    }

    #[test]
    fn strict_records_reject_duplicate_decoded_keys_at_any_depth() {
        let rejected: Vec<String> = vec![
            // Top-level literal duplicate.
            r#"{"role":"assistant","role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#.into(),
            // Literal plus escaped equivalent duplicate.
            r#"{"role":"assistant","\u0072ole":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#.into(),
            // Duplicate inside a nested object.
            r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#.into(),
            // Duplicate inside an object nested in an array.
            r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}},"extra":[{"x":1,"x":2}]}"#.into(),
            // Embedded-NUL-equal keys duplicate.
            r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}},"a\u0000b":1,"a\u0000b":2}"#.into(),
        ];
        for (index, raw) in rejected.iter().enumerate() {
            assert_record_parity(index, 1_000, raw, false);
        }
        let accepted: Vec<String> = vec![
            // The same key in different object instances stays valid.
            r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}},"x":{"created":1},"y":{"created":2}}"#.into(),
            // Case-distinct keys stay valid.
            r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}},"Role":"user"}"#.into(),
            // Normalization-distinct keys stay valid (U+00E9 vs U+0065 U+0301).
            r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}},"caf\u00e9":1,"cafe\u0301":2}"#.into(),
            // Embedded-NUL-distinct keys stay valid.
            r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}},"a\u0000b":1,"a\u0000c":2}"#.into(),
        ];
        for (index, raw) in accepted.iter().enumerate() {
            assert_record_parity(index + 100, 1_000, raw, true);
        }
    }

    #[test]
    fn available_parity_escaped_keys_formatting_and_decoy_keys() {
        let cases: &[(&str, bool)] = &[
            // Escaped key decodes to the exact required name.
            (r#"{"role":"assistant","providerID":"openai","model\u0049D":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#, true),
            // Pretty-printed formatting between tokens is irrelevant.
            ("{\n  \"role\": \"assistant\",\n  \"providerID\": \"openai\",\n  \"modelID\": \"m\",\n  \"time\": { \"created\": 1000, \"completed\": 1001 },\n  \"finish\": \"stop\",\n  \"tokens\": { \"input\": 10, \"output\": 20, \"reasoning\": 5, \"cache\": { \"read\": 3, \"write\": 2 } }\n}", true),
            // Shuffled key order plus decoy extra keys stay valid.
            (r#"{"role":"assistant","finish":"stop","providerID":"openai","modelID":"m","time":{"completed":1001,"created":1000},"tokens":{"cache":{"write":2,"read":3},"reasoning":5,"input":10,"output":20},"extra":"decoy"}"#, true),
            // A decoy key does not satisfy a required field.
            (r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"inputx":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#, false),
        ];
        for (index, (raw, expect)) in cases.iter().enumerate() {
            assert_record_parity(index + 200, 1_000, raw, *expect);
        }
    }

    #[test]
    fn available_parity_surrogates_nul_and_excessive_nesting() {
        let mut cases: Vec<(String, bool)> = vec![
            // Lone high surrogate in a string field is invalid JSON.
            (r#"{"role":"assistant","providerID":"openai","modelID":"\ud800","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#.into(), false),
            // Lone low surrogate in a string field is invalid JSON.
            (r#"{"role":"assistant","providerID":"\ude00","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#.into(), false),
            // Lone surrogate in a key is invalid JSON.
            (r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}},"\ud800":1}"#.into(), false),
            // A valid surrogate pair decodes to one scalar.
            (r#"{"role":"assistant","providerID":"openai","modelID":"\ud83d\ude00","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#.into(), true),
        ];
        let base = r#"{"role":"assistant","providerID":"openai","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#;
        cases.push((
            format!(
                "{}{}{}{}",
                &base[..base.len() - 3],
                "}},\"extra\":",
                "[".repeat(200),
                format!("1{}", "]".repeat(200)).as_str(),
            ) + "}",
            false,
        ));
        cases.push((
            format!(
                "{}{}{}{}",
                &base[..base.len() - 3],
                "}},\"extra\":",
                "[".repeat(50),
                format!("1{}", "]".repeat(50)).as_str(),
            ) + "}",
            true,
        ));
        for (index, (raw, expect)) in cases.iter().enumerate() {
            assert_record_parity(index + 300, 1_000, raw, *expect);
        }

        // An embedded NUL keeps the record strictly valid but the
        // provider unsupported — classified as unsupported, not malformed.
        let nul_provider = r#"{"role":"assistant","providerID":"openai\u0000","modelID":"m","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#;
        assert!(
            parse_record(1_000, nul_provider).is_some(),
            "a NUL-suffixed provider id is still a strictly valid record"
        );
        let path = write_db(&temp_dir("nul-provider"), &[(1_000, "m", nul_provider)]);
        assert!(!available_from(&path), "a NUL-suffixed provider answers no");
        let out = collect_from(&Config::default(), &path, ms(0), ms(10_000));
        assert_eq!(out.events.len(), 0);
        assert_eq!(
            out.notes,
            vec!["opencode: skipped 1 local usage record(s) from unsupported provider(s)"]
        );
        // NUL in the model id stays eligible end to end.
        let nul_model = r#"{"role":"assistant","providerID":"openai","modelID":"model-a\u0000","time":{"created":1000,"completed":1001},"finish":"stop","tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":3,"write":2}}}"#;
        assert_record_parity(399, 1_000, nul_model, true);
    }

    #[test]
    fn strict_decoder_rejects_duplicate_keys_at_any_depth() {
        for raw in [
            r#"{"a":1,"a":2}"#,
            r#"{"a":1,"\u0061":2}"#,
            r#"{"a":{"b":1,"b":2}}"#,
            r#"{"a":[{"b":1,"b":2}]}"#,
            r#"{"a\u0000b":1,"a\u0000b":2}"#,
            r#"{"a":1,"b":{"c":2,"c":3}}"#,
        ] {
            assert!(
                decode_strict(raw).is_none(),
                "duplicate decoded keys must be rejected: {raw}"
            );
        }
        for raw in [
            r#"{"a":1,"b":2}"#,
            r#"{"a":{"a":1},"b":{"a":2}}"#,
            r#"{"A":1,"a":2}"#,
            r#"{"caf\u00e9":1,"cafe\u0301":2}"#,
            r#"{"a\u0000b":1,"a\u0000c":2}"#,
            r#"{ "a" : 1 , "b" : 2 }"#,
            r#"{"\u0061":1,"b":2}"#,
        ] {
            assert!(
                decode_strict(raw).is_some(),
                "distinct decoded keys must be accepted: {raw}"
            );
        }
    }

    #[test]
    fn strict_decoder_rejects_excessive_nesting_and_out_of_range_numbers() {
        let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        assert!(
            decode_strict(&deep).is_none(),
            "200 array levels must exceed the serde recursion limit"
        );
        let ok = format!("{}1{}", "[".repeat(50), "]".repeat(50));
        assert!(decode_strict(&ok).is_some(), "50 levels stay valid");
        let deep_object = format!("{}0{}", r#"{"a":"#.repeat(200), "}".repeat(200));
        assert!(
            decode_strict(&deep_object).is_none(),
            "200 object levels must exceed the serde recursion limit"
        );
        assert!(
            decode_strict(r#"{"a":1e400}"#).is_none(),
            "an out-of-range exponent is invalid JSON"
        );
        assert!(
            decode_strict(r#"{"a":1e308}"#).is_some(),
            "an in-range exponent is valid"
        );
        assert!(
            decode_strict(r#"{"a":18446744073709551615}"#).is_some(),
            "u64::MAX decodes"
        );
        // 2^64 and lexical -0 decode exactly like serde_json (f64
        // numbers) and are rejected by the record predicate's as_u64.
        let p64 = decode_strict(r#"{"a":18446744073709551616}"#).expect("2^64 decodes as f64");
        assert!(p64["a"].as_u64().is_none(), "2^64 is not a u64");
        let neg_zero = decode_strict(r#"{"a":-0}"#).expect("-0 decodes as f64");
        assert!(neg_zero["a"].as_u64().is_none(), "lexical -0 is not a u64");
        assert!(
            decode_strict(r#"{"a":"\ud800"}"#).is_none(),
            "a lone high surrogate is rejected"
        );
        assert!(
            decode_strict(r#"{"a":"\ud83d\ude00"}"#).is_some(),
            "a valid surrogate pair decodes"
        );
    }

    #[test]
    fn status_eligible_uses_shared_predicate_plus_provider_allowlist() {
        let known = valid_data("openai", "model-a", 1_000);
        assert!(status_eligible(1_000, &known.to_string()));
        let unknown = valid_data("private-unknown", "model-a", 1_000);
        assert!(!status_eligible(1_000, &unknown.to_string()));
        assert!(
            parse_record(1_000, &unknown.to_string()).is_some(),
            "a strictly valid unknown provider stays a valid record — unsupported, not malformed"
        );
        let generic = valid_data("opencode", "model-a", 1_000);
        assert!(!status_eligible(1_000, &generic.to_string()));
        let mismatch = valid_data("openai", "model-a", 999);
        assert!(!status_eligible(1_000, &mismatch.to_string()));
        let duplicate = known.to_string().replacen(
            r#""providerID":"openai""#,
            r#""providerID":"openai","providerID":"openai""#,
            1,
        );
        assert!(
            !status_eligible(1_000, &duplicate),
            "duplicate keys answer false"
        );
    }

    #[test]
    fn available_busy_db_waits_250ms_then_answers_no() {
        let path = write_values(
            &temp_dir("avail-busy"),
            &[(1_000, "m1", valid_data("openai", "model-a", 1_000))],
        );
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let start = Instant::now();
        let answer = available_from(&path);
        let elapsed = start.elapsed();
        assert!(!answer, "a busy database answers no");
        assert!(
            elapsed >= Duration::from_millis(250),
            "the busy bound is honored (NFR-3): {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "still bounded: {elapsed:?}"
        );
    }
}
