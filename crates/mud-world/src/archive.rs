//! The events archive: JSON lines, one file per UTC month.
//!
//! Retention never deletes history outright (ADR 0011). Before the store
//! trims old rows of `events`, it appends them here, one line per event in
//! seq order, to `events-YYYY-MM.jsonl` (the month of each event's own
//! `at_ms`, UTC), and fsyncs each file (and the directory, when it made a
//! file) before the delete commits.
//!
//! A row joins a file only above the file's last line
//! ([`archive_admits`]), so a crash between the append and the delete, then
//! a re-run of the same batch, writes nothing twice: the proof is
//! `lemma_crash_rerun_is_idempotent` in `formal/verus/src/durability.rs`. A
//! last line torn by a crash mid-write is cut off before anything is
//! appended.
//!
//! The archive is the basis of the roadmap's admin rollback (restore
//! `world.db` to a point, then replay the archive): see `docs/roadmap.md`.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::durability::archive_admits;
use crate::rewards::DAY_MS;

/// One stored event, as the archive writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRow {
    /// Its seq.
    pub seq: u64,
    /// When it happened, by the game clock.
    pub at_ms: u64,
    /// Its JSON, as `events.body` holds it: one line.
    pub body: String,
}

/// The civil date (year, month) of a day count since 1970-01-01.
const fn year_month(days: i64) -> (i64, u32) {
    // Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "m is 1..=12 by construction"
    )]
    (y, m as u32)
}

/// The file name of the month `at_ms` falls in, UTC.
#[must_use]
pub fn file_name(at_ms: u64) -> String {
    let days = i64::try_from(at_ms / DAY_MS).unwrap_or(i64::MAX / 2);
    let (y, m) = year_month(days);
    format!("events-{y:04}-{m:02}.jsonl")
}

/// The seq of the last complete line of `f`, cutting off a torn last line
/// (one with no newline) first. `None` for an empty file.
fn last_seq(f: &mut File, path: &Path) -> io::Result<Option<u64>> {
    let len = f.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(None);
    }
    // Read backwards until the tail holds a complete last line and the
    // newline before it (or the start of the file).
    let mut tail: Vec<u8> = Vec::new();
    let mut pos = len;
    loop {
        let step = pos.min(8_192);
        pos -= step;
        f.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0; usize::try_from(step).unwrap_or(8_192)];
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&tail);
        tail = chunk;
        let complete = tail.iter().rposition(|b| *b == b'\n');
        // The newline that ends the last complete line, and one before it
        // (or the start of the file).
        let enough = complete.map_or(pos == 0, |end| pos == 0 || tail[..end].contains(&b'\n'));
        if enough {
            break;
        }
    }
    let Some(end) = tail.iter().rposition(|b| *b == b'\n') else {
        // Nothing but a torn line: start the file again.
        f.set_len(0)?;
        return Ok(None);
    };
    let torn = tail.len() - end - 1;
    if torn > 0 {
        f.set_len(len - u64::try_from(torn).unwrap_or(0))?;
        f.sync_all()?;
    }
    let start = tail[..end]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    let line = &tail[start..end];
    let value: serde_json::Value = serde_json::from_slice(line).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: the last line is not an event: {e}", path.display()),
        )
    })?;
    value["seq"].as_u64().map(Some).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: the last line has no seq", path.display()),
        )
    })
}

/// Makes `dir` durable: fsyncs it, so a file just made in it survives a
/// crash.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Appends `rows` (in seq order) to the archive in `dir`, skipping every
/// row at or below its file's last line, then fsyncs what it touched.
/// Returns how many lines it wrote.
///
/// # Errors
/// The directory or a file cannot be made, read, written or synced, or a
/// file's last line is not an archived event. Nothing may be trimmed then.
pub fn append(dir: &Path, rows: &[ArchiveRow]) -> io::Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            sync_dir(parent)?;
        }
    }
    let mut open: BTreeMap<String, (File, Option<u64>)> = BTreeMap::new();
    let mut made = false;
    let mut written = 0;
    for row in rows {
        let name = file_name(row.at_ms);
        if !open.contains_key(&name) {
            let path: PathBuf = dir.join(&name);
            made |= !path.exists();
            let mut f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            let last = last_seq(&mut f, &path)?;
            f.seek(SeekFrom::End(0))?;
            open.insert(name.clone(), (f, last));
        }
        if let Some((f, last)) = open.get_mut(&name)
            && archive_admits(*last, row.seq)
        {
            let mut line = Vec::with_capacity(row.body.len() + 1);
            line.extend_from_slice(row.body.as_bytes());
            line.push(b'\n');
            f.write_all(&line)?;
            *last = Some(row.seq);
            written += 1;
        }
    }
    for (f, _) in open.values_mut() {
        f.flush()?;
        f.sync_all()?;
    }
    if made {
        sync_dir(dir)?;
    }
    Ok(written)
}

/// Every seq in the archive in `dir`, file by file in name order, for
/// tests and the rollback tool.
///
/// # Errors
/// A file cannot be read or a line does not decode.
pub fn seqs(dir: &Path) -> io::Result<Vec<u64>> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    names.sort();
    let mut out = Vec::new();
    for p in names {
        for line in std::fs::read_to_string(&p)?.lines() {
            let v: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            out.push(v["seq"].as_u64().unwrap_or(0));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(seq: u64, at_ms: u64) -> ArchiveRow {
        ArchiveRow {
            seq,
            at_ms,
            body: format!(r#"{{"seq":{seq},"at_ms":{at_ms},"untrusted_text":"a\nb"}}"#),
        }
    }

    #[test]
    fn months_are_named_by_the_utc_calendar() {
        assert_eq!(file_name(0), "events-1970-01.jsonl");
        // 2026-11-24T00:00:00Z.
        assert_eq!(file_name(1_795_478_400_000), "events-2026-11.jsonl");
        // 2024-02-29T23:59:59Z, then 2024-03-01T00:00:00Z.
        assert_eq!(file_name(1_709_251_199_000), "events-2024-02.jsonl");
        assert_eq!(file_name(1_709_251_200_000), "events-2024-03.jsonl");
        assert_eq!(file_name(u64::MAX), file_name(u64::MAX));
    }

    #[test]
    fn a_rerun_after_a_crash_writes_nothing_twice() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().join("archive");
        let jan = 1_767_225_600_000; // 2026-01-01
        let feb = jan + 31 * DAY_MS;
        let batch: Vec<ArchiveRow> = vec![row(1, jan), row(2, jan + 5), row(3, feb), row(4, feb)];
        // A crash after the first two rows were written and synced.
        assert_eq!(append(&arch, &batch[..2]).unwrap(), 2);
        // The re-run of the whole batch.
        assert_eq!(append(&arch, &batch).unwrap(), 2);
        assert_eq!(append(&arch, &batch).unwrap(), 0);
        assert_eq!(seqs(&arch).unwrap(), vec![1, 2, 3, 4]);
        let text = std::fs::read_to_string(arch.join("events-2026-01.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 2, "one line per event: {text}");
    }

    #[test]
    fn a_torn_last_line_is_cut_before_appending() {
        let dir = tempfile::tempdir().unwrap();
        let arch = dir.path().to_path_buf();
        append(&arch, &[row(7, 0)]).unwrap();
        let path = arch.join(file_name(0));
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(br#"{"seq":8,"at_m"#).unwrap();
        drop(f);
        assert_eq!(append(&arch, &[row(8, 0), row(9, 0)]).unwrap(), 2);
        assert_eq!(seqs(&arch).unwrap(), vec![7, 8, 9]);
        // A file that holds only a torn line starts again.
        let other = arch.join(file_name(40 * DAY_MS));
        std::fs::write(&other, b"{\"seq\":").unwrap();
        assert_eq!(append(&arch, &[row(10, 40 * DAY_MS)]).unwrap(), 1);
        assert_eq!(seqs(&arch).unwrap(), vec![7, 8, 9, 10]);
    }

    #[test]
    fn a_long_last_line_is_read_across_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let big = ArchiveRow {
            seq: 3,
            at_ms: 0,
            body: format!(r#"{{"seq":3,"pad":"{}"}}"#, "x".repeat(20_000)),
        };
        append(dir.path(), &[row(2, 0), big]).unwrap();
        assert_eq!(append(dir.path(), &[row(3, 0), row(4, 0)]).unwrap(), 1);
        assert_eq!(seqs(dir.path()).unwrap(), vec![2, 3, 4]);
    }

    #[test]
    fn a_foreign_last_line_refuses_the_append() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(file_name(0)), b"not json\n").unwrap();
        let err = append(dir.path(), &[row(1, 0)]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        std::fs::write(dir.path().join(file_name(0)), b"{\"x\":1}\n").unwrap();
        assert!(append(dir.path(), &[row(1, 0)]).is_err());
        assert_eq!(append(dir.path(), &[]).unwrap(), 0);
    }
}
