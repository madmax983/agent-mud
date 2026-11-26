//! `accounts.db`: its own SQLite file, beside `world.db`, never inside it.
//!
//! The world store writes one transaction per command and sets no busy
//! timeout, so a second writer there would risk failed world saves; and
//! hashes must never ship in a world snapshot.
//!
//! Every query runs on a blocking thread through [`AccountStore::call`],
//! never on a runtime worker.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::{Connection, ErrorCode, OptionalExtension, Transaction, params};

use crate::device::Series;
use crate::error::AccountError;
use crate::name::NameKey;

/// The schema this code writes.
pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
  name_key TEXT PRIMARY KEY, name TEXT NOT NULL, entity TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  origin TEXT NOT NULL CHECK (origin IN ('created','claimed','reset')),
  created_ms INTEGER NOT NULL, password_changed_ms INTEGER NOT NULL, last_login_ms INTEGER,
  sessions_revoked_ms INTEGER NOT NULL DEFAULT 0,
  must_change INTEGER NOT NULL DEFAULT 0,
  locked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS devices (
  series TEXT PRIMARY KEY,
  name_key TEXT NOT NULL REFERENCES accounts(name_key) ON DELETE CASCADE,
  token_hash TEXT NOT NULL,
  persist INTEGER NOT NULL,
  created_ms INTEGER NOT NULL, last_used_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS devices_by_name ON devices (name_key, last_used_ms);
CREATE TABLE IF NOT EXISTS known_addresses (
  name_key TEXT NOT NULL REFERENCES accounts(name_key) ON DELETE CASCADE,
  bucket_hash TEXT NOT NULL,
  last_ok_ms INTEGER NOT NULL,
  PRIMARY KEY (name_key, bucket_hash)
);
CREATE TABLE IF NOT EXISTS accounts_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

pub(crate) fn to_i64(x: u64) -> i64 {
    i64::try_from(x).unwrap_or(i64::MAX)
}

pub(crate) fn to_u64(x: i64) -> u64 {
    u64::try_from(x).unwrap_or(0)
}

/// How an account came to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A new character made at the login prompt.
    Created,
    /// A character saved before accounts, claimed by its first password.
    Claimed,
    /// Opened by an operator's `mud account reset`.
    Reset,
}

impl Origin {
    /// The stored word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Claimed => "claimed",
            Self::Reset => "reset",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "claimed" => Self::Claimed,
            "reset" => Self::Reset,
            _ => Self::Created,
        }
    }
}

/// One account row. Its `Debug` leaves the hash out.
#[derive(Clone)]
pub struct AccountRow {
    /// The key.
    pub key: NameKey,
    /// The character's name as typed when it was made.
    pub name: String,
    /// The bcrypt hash.
    pub password_hash: String,
    /// How it came to be.
    pub origin: Origin,
    /// When it was made.
    pub created_ms: u64,
    /// When the password last changed.
    pub password_changed_ms: u64,
    /// The last good login.
    pub last_login_ms: Option<u64>,
    /// Live sessions that began before this end at revalidation.
    pub sessions_revoked_ms: u64,
    /// The next login must choose a new password.
    pub must_change: bool,
    /// Logins are refused.
    pub locked: bool,
}

impl std::fmt::Debug for AccountRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountRow")
            .field("key", &self.key)
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("locked", &self.locked)
            .field("must_change", &self.must_change)
            .finish_non_exhaustive()
    }
}

/// One remembered browser. Its `Debug` leaves the hash out.
#[derive(Clone)]
pub struct DeviceRow {
    /// Its series.
    pub series: Series,
    /// Its account.
    pub key: NameKey,
    /// The SHA-256 of its token.
    pub token_hash: String,
    /// "Stay signed in" (else a tab token).
    pub persist: bool,
    /// Last resumed or refreshed.
    pub last_used_ms: u64,
    /// It stops resuming at this time.
    pub expires_ms: u64,
}

impl std::fmt::Debug for DeviceRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceRow")
            .field("key", &self.key)
            .field("persist", &self.persist)
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

/// One line of `mud account list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// The name.
    pub name: String,
    /// How it came to be.
    pub origin: Origin,
    /// When it was made.
    pub created_ms: u64,
    /// The last good login.
    pub last_login_ms: Option<u64>,
    /// Remembered browsers.
    pub devices: u64,
    /// Locked.
    pub locked: bool,
    /// Must choose a new password.
    pub must_change: bool,
}

/// The account database.
#[derive(Debug, Clone)]
pub struct AccountStore {
    /// The connection; `None` once [`AccountStore::close`] ran.
    conn: Arc<Mutex<Option<Connection>>>,
}

/// Creates `path` readable by its owner only.
fn create_private(path: &Path) -> Result<(), AccountError> {
    let err = |source| AccountError::Create {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600);
    }
    o.open(path).map(drop).map_err(err)
}

fn init(conn: &Connection, busy_timeout_ms: u32) -> Result<(), AccountError> {
    conn.busy_timeout(std::time::Duration::from_millis(u64::from(busy_timeout_ms)))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(AccountError::Schema { found });
    }
    conn.execute_batch(SCHEMA)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

impl AccountStore {
    /// Opens or creates `accounts.db` at `path`. Returns the store and
    /// whether the file was new.
    ///
    /// # Errors
    /// The file cannot be made or opened, or holds a newer schema.
    pub fn open(path: &Path, busy_timeout_ms: u32) -> Result<(Self, bool), AccountError> {
        let created = !path.exists();
        if created {
            create_private(path)?;
        }
        let conn = Connection::open(path)?;
        init(&conn, busy_timeout_ms)?;
        Ok((
            Self {
                conn: Arc::new(Mutex::new(Some(conn))),
            },
            created,
        ))
    }

    /// A store in memory, for tests and in-memory worlds.
    ///
    /// # Errors
    /// SQLite failed.
    pub fn open_in_memory() -> Result<Self, AccountError> {
        let conn = Connection::open_in_memory()?;
        init(&conn, 0)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(Some(conn))),
        })
    }

    /// Runs `f` under the connection lock on a blocking thread.
    ///
    /// # Errors
    /// `f` failed, or the blocking job died.
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, AccountError> + Send + 'static,
    ) -> Result<T, AccountError> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut c = conn.lock().unwrap_or_else(PoisonError::into_inner);
            c.as_mut().map_or(Err(AccountError::Closed), f)
        })
        .await?
    }

    /// Closes the file at shutdown: checkpoints the write-ahead log into
    /// `accounts.db` (`PRAGMA wal_checkpoint(TRUNCATE)`) and closes the
    /// connection, which removes the log. Later calls fail with
    /// [`AccountError::Closed`]; a second close does nothing.
    ///
    /// # Errors
    /// The checkpoint or the close failed, or another connection held the
    /// log ([`AccountError::Busy`]); the connection is dropped all the
    /// same.
    pub async fn close(&self) -> Result<(), AccountError> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let taken = conn.lock().unwrap_or_else(PoisonError::into_inner).take();
            let Some(c) = taken else {
                return Ok(());
            };
            let checkpoint = c
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .map_err(AccountError::from)
                .and_then(|(busy, log, checkpointed)| {
                    // A blocked checkpoint is a row with busy = 1, not an
                    // error; a file not in WAL mode answers -1 for both.
                    if busy == 0 && checkpointed >= log {
                        Ok(())
                    } else {
                        Err(AccountError::Busy { log, checkpointed })
                    }
                });
            let closed = c.close().map_err(|(_, e)| AccountError::from(e));
            checkpoint.and(closed)
        })
        .await?
    }
}

/// The path of `accounts.db` beside a world file.
#[must_use]
pub fn beside(world_db: &Path) -> PathBuf {
    world_db.with_file_name("accounts.db")
}

fn is_constraint(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation)
}

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
    let key: String = r.get(0)?;
    Ok(AccountRow {
        key: NameKey::parse(&key).ok_or(rusqlite::Error::InvalidQuery)?,
        name: r.get(1)?,
        password_hash: r.get(2)?,
        origin: Origin::parse(&r.get::<_, String>(3)?),
        created_ms: to_u64(r.get(4)?),
        password_changed_ms: to_u64(r.get(5)?),
        last_login_ms: r.get::<_, Option<i64>>(6)?.map(to_u64),
        sessions_revoked_ms: to_u64(r.get(7)?),
        must_change: r.get::<_, i64>(8)? != 0,
        locked: r.get::<_, i64>(9)? != 0,
    })
}

const ROW_COLUMNS: &str = "name_key, name, password_hash, origin, created_ms, password_changed_ms, last_login_ms, sessions_revoked_ms, must_change, locked";

/// The account of `key`.
pub(crate) fn get(conn: &Connection, key: &NameKey) -> Result<Option<AccountRow>, AccountError> {
    Ok(conn
        .query_row(
            &format!("SELECT {ROW_COLUMNS} FROM accounts WHERE name_key = ?1"),
            params![key.as_str()],
            row_of,
        )
        .optional()?)
}

/// Whether (key, bucket) logged in before.
pub(crate) fn is_known(
    conn: &Connection,
    key: &NameKey,
    bucket_hash: Option<&str>,
) -> Result<bool, AccountError> {
    let Some(h) = bucket_hash else {
        return Ok(false);
    };
    Ok(conn
        .query_row(
            "SELECT 1 FROM known_addresses WHERE name_key = ?1 AND bucket_hash = ?2",
            params![key.as_str(), h],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// A new account row.
pub(crate) struct NewAccount<'a> {
    pub key: &'a NameKey,
    pub name: &'a str,
    pub hash: &'a str,
    pub origin: Origin,
    pub now: u64,
    pub must_change: bool,
}

/// Inserts an account and its first known address. `false`: the name was
/// taken meanwhile.
pub(crate) fn insert(
    conn: &mut Connection,
    a: &NewAccount<'_>,
    bucket_hash: Option<&str>,
    known_cap: u32,
) -> Result<bool, AccountError> {
    let tx = conn.transaction()?;
    let done = tx.execute(
        "INSERT INTO accounts (name_key, name, entity, password_hash, origin, created_ms, password_changed_ms, last_login_ms, sessions_revoked_ms, must_change, locked)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?6, ?8, 0)",
        params![
            a.key.as_str(),
            a.name,
            a.key.entity().as_str(),
            a.hash,
            a.origin.as_str(),
            to_i64(a.now),
            (!a.must_change).then(|| to_i64(a.now)),
            i64::from(a.must_change),
        ],
    );
    match done {
        Ok(_) => {}
        Err(e) if is_constraint(&e) => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    if !a.must_change {
        upsert_known(&tx, a.key, bucket_hash, a.now, known_cap)?;
    }
    tx.commit()?;
    Ok(true)
}

/// Records a good address for `key`, keeping the newest `cap`.
fn upsert_known(
    tx: &Transaction<'_>,
    key: &NameKey,
    bucket_hash: Option<&str>,
    now: u64,
    cap: u32,
) -> Result<(), AccountError> {
    let Some(h) = bucket_hash else {
        return Ok(());
    };
    if cap == 0 {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO known_addresses (name_key, bucket_hash, last_ok_ms) VALUES (?1, ?2, ?3)
         ON CONFLICT (name_key, bucket_hash) DO UPDATE SET last_ok_ms = excluded.last_ok_ms",
        params![key.as_str(), h, to_i64(now)],
    )?;
    tx.execute(
        "DELETE FROM known_addresses WHERE name_key = ?1 AND bucket_hash NOT IN
           (SELECT bucket_hash FROM known_addresses WHERE name_key = ?1
            ORDER BY last_ok_ms DESC, bucket_hash LIMIT ?2)",
        params![key.as_str(), i64::from(cap)],
    )?;
    Ok(())
}

/// A good login: the time, the address, a re-hash if one is due, and the
/// sweep of expired devices, in one transaction.
pub(crate) fn record_login(
    conn: &mut Connection,
    key: &NameKey,
    now: u64,
    bucket_hash: Option<&str>,
    known_cap: u32,
    rehash: Option<&str>,
) -> Result<(), AccountError> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE accounts SET last_login_ms = ?2 WHERE name_key = ?1",
        params![key.as_str(), to_i64(now)],
    )?;
    upsert_known(&tx, key, bucket_hash, now, known_cap)?;
    if let Some(h) = rehash {
        tx.execute(
            "UPDATE accounts SET password_hash = ?2 WHERE name_key = ?1",
            params![key.as_str(), h],
        )?;
    }
    tx.execute(
        "DELETE FROM devices WHERE expires_ms <= ?1",
        params![to_i64(now)],
    )?;
    tx.commit()?;
    Ok(())
}

/// Deletes every expired device.
pub(crate) fn sweep(conn: &Connection, now: u64) -> Result<usize, AccountError> {
    Ok(conn.execute(
        "DELETE FROM devices WHERE expires_ms <= ?1",
        params![to_i64(now)],
    )?)
}

/// A new device row.
pub(crate) struct NewDevice<'a> {
    pub key: &'a NameKey,
    pub series: &'a Series,
    pub token_hash: &'a str,
    pub persist: bool,
    pub now: u64,
    pub expires_ms: u64,
}

/// Remembers a device, keeping the newest `cap` of the account.
pub(crate) fn insert_device(
    conn: &mut Connection,
    d: &NewDevice<'_>,
    cap: u32,
) -> Result<(), AccountError> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO devices (series, name_key, token_hash, persist, created_ms, last_used_ms, expires_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        params![
            d.series.as_str(),
            d.key.as_str(),
            d.token_hash,
            i64::from(d.persist),
            to_i64(d.now),
            to_i64(d.expires_ms)
        ],
    )?;
    tx.execute(
        "DELETE FROM devices WHERE name_key = ?1 AND series NOT IN
           (SELECT series FROM devices WHERE name_key = ?1
            ORDER BY last_used_ms DESC, created_ms DESC, series LIMIT ?2)",
        params![d.key.as_str(), i64::from(cap.max(1))],
    )?;
    tx.commit()?;
    Ok(())
}

/// The device of `series`.
pub(crate) fn device(
    conn: &Connection,
    series: &Series,
) -> Result<Option<DeviceRow>, AccountError> {
    Ok(conn
        .query_row(
            "SELECT series, name_key, token_hash, persist, last_used_ms, expires_ms FROM devices WHERE series = ?1",
            params![series.as_str()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?
        .and_then(|(s, k, h, p, used, exp)| {
            Some(DeviceRow {
                series: Series::new(s),
                key: NameKey::parse(&k)?,
                token_hash: h,
                persist: p != 0,
                last_used_ms: to_u64(used),
                expires_ms: to_u64(exp),
            })
        }))
}

/// Slides a device's expiry.
pub(crate) fn slide_device(
    conn: &Connection,
    series: &Series,
    now: u64,
    expires_ms: u64,
) -> Result<(), AccountError> {
    conn.execute(
        "UPDATE devices SET last_used_ms = ?2, expires_ms = ?3 WHERE series = ?1",
        params![series.as_str(), to_i64(now), to_i64(expires_ms)],
    )?;
    Ok(())
}

/// Forgets one device.
pub(crate) fn delete_device(conn: &Connection, series: &Series) -> Result<(), AccountError> {
    conn.execute(
        "DELETE FROM devices WHERE series = ?1",
        params![series.as_str()],
    )?;
    Ok(())
}

/// Forgets every device of `key` but `except`.
pub(crate) fn delete_devices(
    conn: &Connection,
    key: &NameKey,
    except: Option<&Series>,
) -> Result<usize, AccountError> {
    Ok(conn.execute(
        "DELETE FROM devices WHERE name_key = ?1 AND series IS NOT ?2",
        params![key.as_str(), except.map(Series::as_str)],
    )?)
}

/// Sets a new password, ending every session older than `now`.
pub(crate) fn set_password(
    conn: &Connection,
    key: &NameKey,
    hash: &str,
    now: u64,
    must_change: bool,
) -> Result<usize, AccountError> {
    Ok(conn.execute(
        "UPDATE accounts SET password_hash = ?2, password_changed_ms = ?3, sessions_revoked_ms = ?3, must_change = ?4
         WHERE name_key = ?1",
        params![key.as_str(), hash, to_i64(now), i64::from(must_change)],
    )?)
}

/// Sets the password a reset demanded, only over the hash `proved`
/// matched, while a change is still due and the account is unlocked.
/// Returns the rows written (0 or 1).
pub(crate) fn finish_reset(
    conn: &Connection,
    key: &NameKey,
    proved: &str,
    hash: &str,
    now: u64,
) -> Result<usize, AccountError> {
    Ok(conn.execute(
        "UPDATE accounts SET password_hash = ?3, password_changed_ms = ?4, sessions_revoked_ms = ?4, must_change = 0
         WHERE name_key = ?1 AND password_hash = ?2 AND must_change = 1 AND locked = 0",
        params![key.as_str(), proved, hash, to_i64(now)],
    )?)
}

/// Locks or unlocks; a lock ends live sessions.
pub(crate) fn set_locked(
    conn: &Connection,
    key: &NameKey,
    locked: bool,
    now: u64,
) -> Result<usize, AccountError> {
    let sql = if locked {
        "UPDATE accounts SET locked = 1, sessions_revoked_ms = ?2 WHERE name_key = ?1"
    } else {
        "UPDATE accounts SET locked = 0 WHERE name_key = ?1 AND ?2 >= 0"
    };
    Ok(conn.execute(sql, params![key.as_str(), to_i64(now)])?)
}

/// Ends every live session of `key`.
pub(crate) fn revoke(conn: &Connection, key: &NameKey, now: u64) -> Result<usize, AccountError> {
    Ok(conn.execute(
        "UPDATE accounts SET sessions_revoked_ms = ?2 WHERE name_key = ?1",
        params![key.as_str(), to_i64(now)],
    )?)
}

/// Every account, by name.
pub(crate) fn list(conn: &Connection) -> Result<Vec<AccountSummary>, AccountError> {
    let mut st = conn.prepare(
        "SELECT a.name, a.origin, a.created_ms, a.last_login_ms, a.locked, a.must_change,
                (SELECT COUNT(*) FROM devices d WHERE d.name_key = a.name_key)
         FROM accounts a ORDER BY a.name_key",
    )?;
    let rows = st.query_map([], |r| {
        Ok(AccountSummary {
            name: r.get(0)?,
            origin: Origin::parse(&r.get::<_, String>(1)?),
            created_ms: to_u64(r.get(2)?),
            last_login_ms: r.get::<_, Option<i64>>(3)?.map(to_u64),
            locked: r.get::<_, i64>(4)? != 0,
            must_change: r.get::<_, i64>(5)? != 0,
            devices: to_u64(r.get(6)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The entity of every account.
pub(crate) fn entities(conn: &Connection) -> Result<HashSet<String>, AccountError> {
    let mut st = conn.prepare("SELECT entity FROM accounts")?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The count of stored hashes per cost.
pub(crate) fn cost_mix(conn: &Connection) -> Result<BTreeMap<u32, u64>, AccountError> {
    let mut st = conn.prepare("SELECT password_hash FROM accounts")?;
    let mut mix = BTreeMap::new();
    for h in st.query_map([], |r| r.get::<_, String>(0))? {
        if let Some(c) = crate::hasher::cost_of(&h?) {
            *mix.entry(c).or_default() += 1;
        }
    }
    Ok(mix)
}

/// A meta value.
pub(crate) fn meta(conn: &Connection, key: &str) -> Result<Option<String>, AccountError> {
    Ok(conn
        .query_row(
            "SELECT value FROM accounts_meta WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?)
}

/// Sets a meta value unless it is set.
pub(crate) fn meta_init(conn: &Connection, key: &str, value: &str) -> Result<String, AccountError> {
    conn.execute(
        "INSERT OR IGNORE INTO accounts_meta (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(meta(conn, key)?.unwrap_or_else(|| value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> NameKey {
        NameKey::parse(s).unwrap()
    }

    fn new<'a>(k: &'a NameKey, name: &'a str) -> NewAccount<'a> {
        NewAccount {
            key: k,
            name,
            hash: "$2b$04$abcdefghijklmnopqrstuuABCDEFGHIJKLMNOPQRSTUVWXYZ01234",
            origin: Origin::Created,
            now: 5,
            must_change: false,
        }
    }

    #[tokio::test]
    async fn a_close_that_another_connection_blocks_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.db");
        let (s, _) = AccountStore::open(&path, 50).unwrap();
        let k = key("ada");
        s.call(move |c| insert(c, &new(&k, "Ada"), None, 5).map(drop))
            .await
            .unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        let k = key("bo");
        s.call(move |c| insert(c, &new(&k, "Bo"), None, 5).map(drop))
            .await
            .unwrap();
        let err = s.close().await.unwrap_err();
        assert!(matches!(err, AccountError::Busy { .. }), "{err}");
        assert!(matches!(
            s.call(|_| Ok(())).await,
            Err(AccountError::Closed)
        ));
        drop(reader);
        let (s, _) = AccountStore::open(&path, 50).unwrap();
        s.close().await.unwrap();
    }

    #[test]
    fn schema_is_version_1_and_newer_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/accounts.db");
        let (s, created) = AccountStore::open(&path, 1_000).unwrap();
        assert!(created);
        drop(s);
        let (_, created) = AccountStore::open(&path, 1_000).unwrap();
        assert!(!created);
        let c = Connection::open(&path).unwrap();
        let v: i64 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        c.pragma_update(None, "user_version", 2).unwrap();
        drop(c);
        let err = AccountStore::open(&path, 1_000).unwrap_err();
        assert!(matches!(err, AccountError::Schema { found: 2 }), "{err}");
        assert_eq!(
            beside(Path::new("data/world.db")),
            Path::new("data/accounts.db")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_db_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.db");
        let _ = AccountStore::open(&path, 1_000).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }

    #[test]
    fn names_are_unique_case_insensitively() {
        let mut c = Connection::open_in_memory().unwrap();
        init(&c, 0).unwrap();
        let (a, b) = (key("Ada"), key("ADA"));
        assert!(insert(&mut c, &new(&a, "Ada"), None, 5).unwrap());
        assert!(!insert(&mut c, &new(&b, "ADA"), None, 5).unwrap());
        assert_eq!(get(&c, &b).unwrap().unwrap().name, "Ada");
        assert!(format!("{:?}", get(&c, &a).unwrap().unwrap()).contains("Ada"));
        assert!(!format!("{:?}", get(&c, &a).unwrap().unwrap()).contains("$2b$"));
    }

    #[test]
    fn devices_cascade_and_the_cap_drops_the_oldest() {
        let mut c = Connection::open_in_memory().unwrap();
        init(&c, 0).unwrap();
        let ada = key("ada");
        insert(&mut c, &new(&ada, "ada"), None, 5).unwrap();
        for i in 0..4u64 {
            let s = Series::new(format!("s{i}"));
            let d = NewDevice {
                key: &ada,
                series: &s,
                token_hash: "h",
                persist: true,
                now: 10 + i,
                expires_ms: 1_000,
            };
            insert_device(&mut c, &d, 3).unwrap();
        }
        assert!(
            device(&c, &Series::new("s0".into())).unwrap().is_none(),
            "the oldest went"
        );
        assert!(device(&c, &Series::new("s3".into())).unwrap().is_some());
        assert_eq!(list(&c).unwrap()[0].devices, 3);
        assert_eq!(sweep(&c, 999).unwrap(), 0);
        c.execute("DELETE FROM accounts WHERE name_key = 'ada'", [])
            .unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "devices cascade with their account");
    }

    #[test]
    fn known_addresses_are_hashed_and_capped() {
        let mut c = Connection::open_in_memory().unwrap();
        init(&c, 0).unwrap();
        let ada = key("ada");
        insert(&mut c, &new(&ada, "ada"), Some("h0"), 2).unwrap();
        for (i, h) in ["h1", "h2"].iter().enumerate() {
            record_login(&mut c, &ada, 10 + i as u64, Some(h), 2, None).unwrap();
        }
        assert!(!is_known(&c, &ada, Some("h0")).unwrap(), "capped at 2");
        assert!(is_known(&c, &ada, Some("h1")).unwrap());
        assert!(is_known(&c, &ada, Some("h2")).unwrap());
        assert!(!is_known(&c, &ada, None).unwrap());
    }

    #[test]
    fn tokens_are_stored_hashed() {
        let mut c = Connection::open_in_memory().unwrap();
        init(&c, 0).unwrap();
        let ada = key("ada");
        insert(&mut c, &new(&ada, "ada"), None, 5).unwrap();
        let f = crate::device::fresh();
        let d = NewDevice {
            key: &ada,
            series: &f.series,
            token_hash: &f.token_hash,
            persist: false,
            now: 1,
            expires_ms: 100,
        };
        insert_device(&mut c, &d, 5).unwrap();
        let (_, token) = crate::device::parse(&f.value).unwrap();
        let all: String = c
            .query_row(
                "SELECT group_concat(series || token_hash) FROM devices",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!all.contains(token.expose()));
        let row = device(&c, &f.series).unwrap().unwrap();
        assert!(crate::device::matches(&token, &row.token_hash));
        assert!(!format!("{row:?}").contains(&row.token_hash));
    }

    #[test]
    fn no_sqlite_on_a_runtime_worker() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let s = AccountStore::open_in_memory().unwrap();
        let worker = std::thread::current().id();
        let ran_on = rt.block_on(s.call(|c| {
            sweep(c, 1)?;
            Ok(std::thread::current().id())
        }));
        assert_ne!(
            ran_on.unwrap(),
            worker,
            "the query ran on a blocking thread"
        );
    }

    #[test]
    fn meta_values_are_set_once() {
        let c = Connection::open_in_memory().unwrap();
        init(&c, 0).unwrap();
        assert_eq!(meta(&c, "x").unwrap(), None);
        assert_eq!(meta_init(&c, "x", "1").unwrap(), "1");
        assert_eq!(meta_init(&c, "x", "2").unwrap(), "1");
        assert!(cost_mix(&c).unwrap().is_empty());
        assert!(entities(&c).unwrap().is_empty());
    }
}
