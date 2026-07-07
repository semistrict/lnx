use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::paths::Layout;

use super::{LIFECYCLE_SEQUENCE, RUN_ID_ENV, lock_file, remove_path_if_exists, unlock_file};

pub(crate) struct TimingLog {
    path: PathBuf,
    state_path: PathBuf,
    base_unix_nanos: u128,
    state: Mutex<TimingState>,
}

pub(crate) struct RunLog {
    pub(crate) path: PathBuf,
    file: Mutex<fs::File>,
}

pub(crate) struct TraceLog {
    pub(crate) path: PathBuf,
    state: Mutex<TraceState>,
}

struct TimingState {
    file: fs::File,
    state_file: fs::File,
}

struct TraceState {
    connection: Connection,
    next_sequence: i64,
}

pub(crate) struct TraceField {
    key: &'static str,
    ordinal: Option<i64>,
    value: TraceValue,
}

enum TraceValue {
    Text(String),
    Integer(i64),
    Blob(Vec<u8>),
}

pub(crate) fn json_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            ch => vec![ch],
        })
        .collect()
}

pub(crate) fn current_run_id() -> String {
    std::env::var(RUN_ID_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| log_value(&value))
        .unwrap_or_else(|| new_lifecycle_id("run"))
}

pub(crate) fn new_lifecycle_id(prefix: &str) -> String {
    let sequence = LIFECYCLE_SEQUENCE.fetch_add(1, Ordering::SeqCst);
    format!(
        "{prefix}-{}-{}-{sequence}",
        unix_nanos(),
        std::process::id()
    )
}

pub(crate) fn log_value(value: &str) -> String {
    value.replace(['\r', '\n', '\t', ' '], "_")
}

pub(crate) fn system_time_unix_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|time| time.as_nanos())
}

impl RunLog {
    pub(crate) fn open(layout: &Layout) -> Result<Self> {
        let path = layout.run_dir.join("lnx.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    pub(crate) fn line(&self, message: impl AsRef<str>) {
        let mut file = match self.file.lock() {
            Ok(file) => file,
            Err(_) => return,
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let message = message.as_ref().replace('\r', "").replace('\n', " | ");
        let _ = writeln!(
            file,
            "{}.{:09} {}",
            now.as_secs(),
            now.subsec_nanos(),
            message
        );
    }
}

impl TraceLog {
    pub(crate) fn open(layout: &Layout) -> Result<Self> {
        let path = layout.run_dir.join("deterministic-trace.sqlite3");
        remove_path_if_exists(&path)?;
        let connection =
            Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
        connection
            .execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA foreign_keys = ON;

                CREATE TABLE trace_metadata (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                ) STRICT;

                CREATE TABLE events (
                    sequence INTEGER PRIMARY KEY NOT NULL,
                    event TEXT NOT NULL
                ) STRICT;

                CREATE INDEX events_event_idx ON events(event);

                CREATE TABLE event_text_fields (
                    sequence INTEGER NOT NULL REFERENCES events(sequence) ON DELETE CASCADE,
                    key TEXT NOT NULL,
                    ordinal INTEGER,
                    value TEXT NOT NULL
                ) STRICT;

                CREATE TABLE event_integer_fields (
                    sequence INTEGER NOT NULL REFERENCES events(sequence) ON DELETE CASCADE,
                    key TEXT NOT NULL,
                    ordinal INTEGER,
                    value INTEGER NOT NULL
                ) STRICT;

                CREATE TABLE event_blob_fields (
                    sequence INTEGER NOT NULL REFERENCES events(sequence) ON DELETE CASCADE,
                    key TEXT NOT NULL,
                    ordinal INTEGER,
                    value BLOB NOT NULL
                ) STRICT;

                CREATE INDEX event_text_fields_lookup_idx
                    ON event_text_fields(sequence, key, ordinal);
                CREATE INDEX event_integer_fields_lookup_idx
                    ON event_integer_fields(sequence, key, ordinal);
                CREATE INDEX event_blob_fields_lookup_idx
                    ON event_blob_fields(sequence, key, ordinal);
                "#,
            )
            .with_context(|| format!("initialize {}", path.display()))?;
        connection
            .execute(
                "INSERT INTO trace_metadata (key, value) VALUES (?1, ?2)",
                params!["format", "lnx-deterministic-trace-v1"],
            )
            .with_context(|| format!("write trace metadata {}", path.display()))?;
        Ok(Self {
            path,
            state: Mutex::new(TraceState {
                connection,
                next_sequence: 0,
            }),
        })
    }

    pub(crate) fn event(&self, event: &str, fields: Vec<TraceField>) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        let sequence = state.next_sequence;
        if insert_trace_event(&mut state.connection, sequence, event, &fields).is_err() {
            return;
        }
        state.next_sequence = state.next_sequence.saturating_add(1);
    }

    pub(crate) fn set_next_sequence(&self, sequence: u64) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        state.next_sequence = i64::try_from(sequence).unwrap_or(i64::MAX);
    }

    pub(crate) fn next_sequence(&self) -> u64 {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return 0,
        };
        u64::try_from(state.next_sequence).unwrap_or(0)
    }
}

fn insert_trace_event(
    connection: &mut Connection,
    sequence: i64,
    event: &str,
    fields: &[TraceField],
) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO events (sequence, event) VALUES (?1, ?2)",
        params![sequence, event],
    )?;
    for field in fields {
        match &field.value {
            TraceValue::Text(value) => {
                transaction.execute(
                    "INSERT INTO event_text_fields (sequence, key, ordinal, value) VALUES (?1, ?2, ?3, ?4)",
                    params![sequence, field.key, field.ordinal, value],
                )?;
            }
            TraceValue::Integer(value) => {
                transaction.execute(
                    "INSERT INTO event_integer_fields (sequence, key, ordinal, value) VALUES (?1, ?2, ?3, ?4)",
                    params![sequence, field.key, field.ordinal, value],
                )?;
            }
            TraceValue::Blob(value) => {
                transaction.execute(
                    "INSERT INTO event_blob_fields (sequence, key, ordinal, value) VALUES (?1, ?2, ?3, ?4)",
                    params![sequence, field.key, field.ordinal, value],
                )?;
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

pub(crate) fn trace_text(key: &'static str, value: impl Into<String>) -> TraceField {
    TraceField {
        key,
        ordinal: None,
        value: TraceValue::Text(value.into()),
    }
}

pub(crate) fn trace_text_ordinal(
    key: &'static str,
    ordinal: usize,
    value: impl Into<String>,
) -> TraceField {
    TraceField {
        key,
        ordinal: Some(ordinal as i64),
        value: TraceValue::Text(value.into()),
    }
}

pub(crate) fn trace_integer(key: &'static str, value: impl Into<i64>) -> TraceField {
    TraceField {
        key,
        ordinal: None,
        value: TraceValue::Integer(value.into()),
    }
}

pub(crate) fn trace_bool(key: &'static str, value: bool) -> TraceField {
    trace_integer(key, if value { 1 } else { 0 })
}

pub(crate) fn trace_blob(key: &'static str, value: &[u8]) -> TraceField {
    TraceField {
        key,
        ordinal: None,
        value: TraceValue::Blob(value.to_vec()),
    }
}

impl TimingLog {
    pub(crate) fn open(
        layout: &Layout,
        command: &[String],
        restore_snapshot: Option<&Path>,
    ) -> Result<Self> {
        let path = layout.run_dir.join("timings.log");
        let state_path = layout.run_dir.join("timings.state");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let mut state_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&state_path)
            .with_context(|| format!("open {}", state_path.display()))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let base_unix_nanos = now.as_nanos();
        write!(state_file, "{base_unix_nanos}")?;
        writeln!(
            file,
            "\nrun pid={} unix={} instance={} restore={} cmd={:?}",
            std::process::id(),
            now.as_secs(),
            layout.instance,
            restore_snapshot
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "false".to_string()),
            command
        )?;
        Ok(Self {
            path,
            state_path,
            base_unix_nanos,
            state: Mutex::new(TimingState { file, state_file }),
        })
    }

    pub(crate) fn install_for_libkrun(&self) {
        // This happens before the libkrun thread is spawned; libkrun reads these
        // process-local values only to append profiling milestones.
        unsafe {
            std::env::set_var("KRUN_TIMINGS_LOG", &self.path);
            std::env::set_var("KRUN_TIMINGS_STATE", &self.state_path);
            std::env::set_var(
                "KRUN_TIMINGS_BASE_UNIX_NANOS",
                self.base_unix_nanos.to_string(),
            );
        }
    }

    pub(crate) fn event(&self, label: &str) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        let now = unix_nanos();
        if lock_file(&state.state_file).is_err() {
            return;
        }
        let delta_nanos = replace_timing_state(&mut state.state_file, self.base_unix_nanos, now)
            .unwrap_or_default();
        let elapsed_nanos = now.saturating_sub(self.base_unix_nanos);

        let line = format!(
            "{:>10.3}ms +{:>9.3}ms {}",
            elapsed_nanos as f64 / 1_000_000.0,
            delta_nanos as f64 / 1_000_000.0,
            label
        );
        let _ = state.file.write_all(line.as_bytes());
        let _ = state.file.write_all(b"\n");
        let _ = unlock_file(&state.state_file);
    }
}

pub(crate) fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn replace_timing_state(file: &mut fs::File, base: u128, now: u128) -> std::io::Result<u128> {
    file.seek(SeekFrom::Start(0))?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    let previous = raw.trim().parse::<u128>().unwrap_or(base);
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{now}")?;
    Ok(now.saturating_sub(previous))
}
