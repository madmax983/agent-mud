//! Exact littleguy spend, read from the daemon's own cost ledger.
//!
//! `lg serve` reports usage only on a finished reply. An op that stops
//! early (no answer), waits for an approval, outlives its wake, or a
//! heartbeat between wakes, still calls the model and reports nothing.
//! autumn-harvest writes every model call to `harvest_llm_ledger` in the
//! daemon's database, in the transaction that records the call, so that
//! ledger is the one complete record. A [`LedgerMeter`] reads it through a
//! read-only handle and charges each row once.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use autumn_harvest_sqlite::{SqliteReader, SqliteResult};

use crate::mind::Usage;

/// One UTC day, in ms.
const DAY_MS: u64 = 86_400_000;

/// A ledger row's identity: run, completion event, call within it.
type RowKey = (String, i64, u32);

/// Reads one daemon's ledger and charges each model call once.
#[derive(Debug)]
pub struct LedgerMeter {
    path: PathBuf,
    /// Rows already accounted, with their completion time; kept for the
    /// read window only.
    seen: Mutex<HashMap<RowKey, u64>>,
}

impl LedgerMeter {
    /// A meter over the littleguy database at `path`. The file need not
    /// exist yet.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// The spend not charged yet, as of `now_ms`; `None` when the daemon
    /// has no database yet (nothing to read, so the caller keeps what the
    /// daemon reported).
    ///
    /// Reads the rows of today and yesterday (UTC). A row seen for the
    /// first time is charged if it falls on today, and only marked if it
    /// falls on an earlier day: that day's budget has closed, and a
    /// restarted MUD must not bill it to today.
    ///
    /// # Errors
    /// The database exists but cannot be read.
    pub fn charge(&self, now_ms: u64) -> SqliteResult<Option<Usage>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let today = now_ms - now_ms % DAY_MS;
        let window = today.saturating_sub(DAY_MS);
        let reader = SqliteReader::open(&self.path)?;
        let rows = reader.llm_ledger_between(
            jiff::Timestamp::from_millisecond(i64::try_from(window).unwrap_or(i64::MAX))
                .unwrap_or(jiff::Timestamp::UNIX_EPOCH),
            jiff::Timestamp::MAX,
        )?;
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.retain(|_, at| *at >= window);
        let mut usage = Usage::default();
        let mut fresh: HashSet<RowKey> = HashSet::new();
        for row in rows {
            let at = u64::try_from(row.recorded_at.as_millisecond()).unwrap_or(0);
            let key = (row.exec_id.to_string(), row.event_seq, row.call_index);
            if seen.contains_key(&key) || !fresh.insert(key.clone()) {
                continue;
            }
            seen.insert(key, at);
            if at >= today {
                usage.calls = usage.calls.saturating_add(1);
                usage.input_tokens = usage.input_tokens.saturating_add(row.input_tokens);
                usage.output_tokens = usage.output_tokens.saturating_add(row.output_tokens);
                usage.cost_usd_micros = usage
                    .cost_usd_micros
                    .saturating_add(row.cost_usd_micros.unwrap_or(0));
            }
        }
        drop(seen);
        Ok(Some(usage))
    }
}

/// A hand-made ledger, for tests here and in [`crate::littleguy`].
#[cfg(test)]
pub(crate) mod fake {
    use std::path::PathBuf;

    use rusqlite::{Connection, params};

    /// A ledger table as autumn-harvest makes it, in `dir/lg.db`.
    pub fn ledger(dir: &std::path::Path) -> (PathBuf, Connection) {
        let path = dir.join("lg.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE harvest_llm_ledger (
                exec_id TEXT NOT NULL, event_seq INTEGER NOT NULL,
                call_index INTEGER NOT NULL, activity_name TEXT NOT NULL,
                model TEXT NOT NULL, input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL, cost_usd_micros INTEGER,
                latency_ms INTEGER NOT NULL, recorded_at INTEGER NOT NULL,
                PRIMARY KEY (exec_id, event_seq, call_index));",
        )
        .unwrap();
        (path, conn)
    }

    /// Records one model call.
    pub fn call(conn: &Connection, seq: i64, input: i64, output: i64, cost: Option<i64>, at: u64) {
        conn.execute(
            "INSERT INTO harvest_llm_ledger VALUES (?1, ?2, 0, 'model', 'm', ?3, ?4, ?5, 1, ?6)",
            params![
                "00000000-0000-0000-0000-000000000001",
                seq,
                input,
                output,
                cost,
                i64::try_from(at).unwrap()
            ],
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{call, ledger};
    use super::*;

    const TODAY: u64 = 20_000 * DAY_MS;

    #[test]
    fn each_call_is_charged_once() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = ledger(dir.path());
        let meter = LedgerMeter::new(&path);
        call(&conn, 1, 1_000, 200, Some(50), TODAY + 10);
        call(&conn, 2, 3_000, 400, None, TODAY + 20);
        let first = meter.charge(TODAY + 30).unwrap();
        assert_eq!(
            first,
            Some(Usage {
                calls: 2,
                input_tokens: 4_000,
                output_tokens: 600,
                cost_usd_micros: 50
            })
        );
        assert_eq!(meter.charge(TODAY + 40).unwrap(), Some(Usage::default()));
        call(&conn, 3, 10, 5, Some(1), TODAY + 50);
        assert_eq!(meter.charge(TODAY + 60).unwrap().unwrap().tokens(), 15);
    }

    #[test]
    fn a_closed_day_is_marked_not_charged() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = ledger(dir.path());
        call(&conn, 1, 500, 500, None, TODAY - 10);
        call(&conn, 2, 7, 3, None, TODAY + 10);
        let meter = LedgerMeter::new(&path);
        assert_eq!(meter.charge(TODAY + 20).unwrap().unwrap().tokens(), 10);
        assert_eq!(meter.charge(TODAY + 30).unwrap(), Some(Usage::default()));
    }

    #[test]
    fn a_call_made_just_before_midnight_is_not_billed_to_the_new_day() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = ledger(dir.path());
        let meter = LedgerMeter::new(&path);
        call(&conn, 1, 100, 0, None, TODAY + DAY_MS - 5);
        assert_eq!(
            meter.charge(TODAY + DAY_MS - 1).unwrap().unwrap().tokens(),
            100
        );
        assert_eq!(
            meter.charge(TODAY + DAY_MS + 1).unwrap(),
            Some(Usage::default())
        );
    }

    #[test]
    fn no_database_yet_is_no_spend() {
        let dir = tempfile::tempdir().unwrap();
        let meter = LedgerMeter::new(dir.path().join("missing.db"));
        assert_eq!(meter.charge(TODAY).unwrap(), None);
    }
}
