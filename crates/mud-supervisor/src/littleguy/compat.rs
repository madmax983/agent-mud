//! An older `lg` binary, read from what it said when it refused its config.
//!
//! lg has no version or features request, so the MUD cannot ask an `lg`
//! what it reads before it writes an `lg.toml`. lg reads its config with
//! `deny_unknown_fields`, so an `lg` built before a section existed exits
//! with status 2 and names the first field it does not know. This module
//! reads that message from the daemon's `lg.log`, strictly: the `toml`
//! error must point at the line and column of that very key in the file
//! the MUD wrote.
//!
//! A refused key the MUD writes only as an extra ([`Optional`]) is dropped
//! for that binary, for the life of the process, and the daemon starts
//! again; each drop is warned once per binary. Any other refused key fails
//! the start with an error that names [`LG_MIN_COMMIT`] and the README.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

/// The first littleguy commit that serves all the MUD needs.
///
/// Its `lg serve` reads every section the MUD writes and speaks every
/// request it sends. CI pins it (`.github/workflows/ci.yml`), and the
/// README and `mud.toml.example` name it; a test checks that all of them
/// agree.
pub const LG_MIN_COMMIT: &str = "c507731f7a8930444931c60169931d657cecec73";

/// The README section that says how to build and install `lg`.
pub const README_INSTALL: &str = "README.md#install-littleguy";

/// The most bytes of `lg.log` read back after a failed start.
const LOG_TAIL_BYTES: u64 = 16 * 1024;

/// A part of `lg.toml` the MUD writes as an extra: an agent still plays
/// without it. An older `lg` that refuses one runs without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Optional {
    /// `[compaction]`: a long op saves notes and starts over.
    Compaction,
    /// The top-level `max_request_bytes`: the request cap.
    MaxRequestBytes,
    /// `research` in `[heartbeat]`: a tick researches open ideas.
    HeartbeatResearch,
}

impl Optional {
    /// Every optional part, in the order `render_config` writes them.
    pub const ALL: [Self; 3] = [
        Self::MaxRequestBytes,
        Self::HeartbeatResearch,
        Self::Compaction,
    ];

    /// Its dotted path in `lg.toml`.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::MaxRequestBytes => "max_request_bytes",
            Self::HeartbeatResearch => "heartbeat.research",
        }
    }

    /// The first littleguy commit that reads it.
    #[must_use]
    pub const fn since(self) -> &'static str {
        match self {
            Self::Compaction | Self::MaxRequestBytes => "littleguy #108 (86309bf)",
            Self::HeartbeatResearch => "littleguy #65 (610a484)",
        }
    }

    /// The `[littleguy]` key of `mud.toml` that leaves it out on purpose.
    #[must_use]
    pub const fn off_key(self) -> &'static str {
        match self {
            Self::Compaction => "compaction = false",
            Self::MaxRequestBytes => "max_request_bytes = 0",
            Self::HeartbeatResearch => "heartbeat_minutes = 0",
        }
    }

    /// The optional part at `path`, if the path names one.
    #[must_use]
    pub fn from_path(path: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.path() == path)
    }
}

/// The parts each `lg` binary was found to lack, by its resolved path.
/// It lives as long as the process: a rebuilt `lg` is seen at the next
/// start of the MUD.
static LACKS: LazyLock<Mutex<HashMap<PathBuf, BTreeSet<Optional>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// What the binary at `bin` was found to lack so far.
#[must_use]
pub fn lacks(bin: &Path) -> BTreeSet<Optional> {
    LACKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(bin)
        .cloned()
        .unwrap_or_default()
}

/// Records that the binary at `bin` lacks `part`. True the first time
/// only, so the caller warns once per binary and part.
pub fn record(bin: &Path, part: Optional) -> bool {
    LACKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(bin.to_path_buf())
        .or_default()
        .insert(part)
}

/// The `lg` binary that `bin` runs: `bin` itself when it names a path, or
/// the first executable file of that name on `PATH`. It is the key of
/// [`lacks`] and the path the warnings name.
#[must_use]
pub fn resolve_bin(bin: &Path) -> PathBuf {
    if bin.components().count() != 1 {
        return bin.to_path_buf();
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(bin))
        .find(|p| is_executable(p))
        .unwrap_or_else(|| bin.to_path_buf())
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The last [`LOG_TAIL_BYTES`] of the log at `path`, or nothing.
#[must_use]
pub fn log_tail(path: &Path) -> String {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map_or(0, |m| m.len());
    if f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::new();
    if f.take(LOG_TAIL_BYTES).read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The dotted path of the key that `lg` refused in `config`, read from its
/// `log`, or `None` when the log holds no such refusal.
///
/// lg prints `error: the config is not valid: ` and the `toml` error:
///
/// ```text
/// error: the config is not valid: TOML parse error at line 28, column 2
///    |
/// 28 | [compaction]
///    |  ^^^^^^^^^^
/// unknown field `compaction`, expected one of `owner`, `system`, `window`, ...
/// ```
///
/// Each part must hold: the excerpt is line 28 of `config` as written, the
/// caret line follows it, and `config` has a key named `compaction` that
/// starts at line 28, column 2. The last such refusal in the log counts.
#[must_use]
pub fn refused_key(log: &str, config: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    lines
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, _)| refusal_at(&lines[i..], config))
}

/// The refusal that starts at `lines[0]`, if it is one.
fn refusal_at(lines: &[&str], config: &str) -> Option<String> {
    const HEAD: &str = "error: the config is not valid: TOML parse error at line ";
    let at = lines.first()?.strip_prefix(HEAD)?;
    let (line, column) = at.split_once(", column ")?;
    let (line, column) = (number(line)?, number(column)?);
    let [gutter, excerpt, carets, message, ..] = lines.get(1..)? else {
        return None;
    };
    if gutter.trim() != "|" {
        return None;
    }
    let source = excerpt
        .trim_start()
        .strip_prefix(&format!("{line} |"))?
        .strip_prefix(' ')
        .unwrap_or("");
    if config.lines().nth(line - 1)?.trim_end() != source.trim_end() {
        return None;
    }
    let marks = carets.trim_start().strip_prefix('|')?.trim();
    if marks.is_empty() || !marks.chars().all(|c| c == '^') {
        return None;
    }
    let rest = message.strip_prefix("unknown field `")?;
    let (field, tail) = rest.split_once('`')?;
    if !(tail.starts_with(", expected ") || tail == ", there are no fields") {
        return None;
    }
    key_at(config, line, column).filter(|path| path.rsplit('.').next() == Some(field))
}

/// A 1-based line or column number with no sign, padding or leading zero.
fn number(text: &str) -> Option<usize> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|n| *n >= 1)
}

/// The dotted path of the key of `config` that starts at `line`,
/// `column` (1-based, in characters), as the `toml` parser spans it.
fn key_at(config: &str, line: usize, column: usize) -> Option<String> {
    let doc = toml::de::DeTable::parse(config).ok()?;
    let mut found = None;
    walk(doc.get_ref(), "", &mut |path, start| {
        if found.is_none() && position(config, start) == (line, column) {
            found = Some(path.to_owned());
        }
    });
    found
}

/// Calls `seen` with the path and key offset of every key in `table`.
fn walk(table: &toml::de::DeTable<'_>, prefix: &str, seen: &mut dyn FnMut(&str, usize)) {
    for (k, v) in table {
        let path = if prefix.is_empty() {
            k.get_ref().to_string()
        } else {
            format!("{prefix}.{}", k.get_ref())
        };
        seen(&path, k.span().start);
        match v.get_ref() {
            toml::de::DeValue::Table(t) => walk(t, &path, seen),
            toml::de::DeValue::Array(items) => {
                for item in items {
                    if let toml::de::DeValue::Table(t) = item.get_ref() {
                        walk(t, &path, seen);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The 1-based line and column (in characters) of byte `offset`.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config of the incident, cut to what matters: `[compaction]` on
    /// line 4, column 2.
    const CONFIG: &str = "owner = \"mud\"\nmax_request_bytes = 240000\n\n[compaction]\nat_percent = 80\n\n[heartbeat]\nevery_minutes = 240\nresearch = true\n";

    /// What an lg built before littleguy #108 wrote to `lg.log` for it.
    fn old_lg_log(line: usize, source: &str, carets: &str, field: &str) -> String {
        let pad = " ".repeat(line.to_string().len());
        format!(
            "\u{1b}[2m2026-11-25T10:00:00Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m lg: starting\n\
             error: the config is not valid: TOML parse error at line {line}, column {col}\n\
             {pad} |\n\
             {line} | {source}\n\
             {pad} | {carets}\n\
             unknown field `{field}`, expected one of `owner`, `system`, `window`, `max_steps`, `tools`, `memory`, `heartbeat`\n\n",
            col = if source.starts_with('[') { 2 } else { 1 },
        )
    }

    #[test]
    fn the_incident_names_compaction() {
        let log = old_lg_log(4, "[compaction]", " ^^^^^^^^^^", "compaction");
        assert_eq!(refused_key(&log, CONFIG).as_deref(), Some("compaction"));
        assert_eq!(
            Optional::from_path("compaction"),
            Some(Optional::Compaction)
        );
    }

    #[test]
    fn a_top_level_key_and_a_nested_key_are_named_by_path() {
        let log = old_lg_log(
            2,
            "max_request_bytes = 240000",
            "^^^^^^^^^^^^^^^^^",
            "max_request_bytes",
        );
        assert_eq!(
            refused_key(&log, CONFIG).as_deref(),
            Some("max_request_bytes")
        );
        let log = old_lg_log(9, "research = true", "^^^^^^^^", "research");
        assert_eq!(
            refused_key(&log, CONFIG).as_deref(),
            Some("heartbeat.research")
        );
        assert_eq!(
            Optional::from_path("heartbeat.research"),
            Some(Optional::HeartbeatResearch)
        );
        assert_eq!(
            Optional::from_path("research"),
            None,
            "lg's [research] is never written"
        );
    }

    /// The message the `toml` crate (lg's own parser) really prints for a
    /// config with a section its struct does not know.
    #[test]
    fn the_real_toml_message_is_read() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldHeartbeat {
            #[serde(rename = "every_minutes")]
            _every: u32,
        }
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldLg {
            #[serde(rename = "owner")]
            _owner: String,
            #[serde(rename = "max_request_bytes")]
            _max: u64,
            #[serde(rename = "heartbeat")]
            _heartbeat: OldHeartbeat,
        }
        let Err(e) = toml::from_str::<OldLg>(CONFIG) else {
            panic!("an old lg refuses [compaction]");
        };
        let log = format!("error: the config is not valid: {e}\n");
        assert_eq!(
            refused_key(&log, CONFIG).as_deref(),
            Some("compaction"),
            "{log}"
        );
    }

    #[test]
    fn anything_else_is_no_refusal() {
        let good = old_lg_log(4, "[compaction]", " ^^^^^^^^^^", "compaction");
        assert!(refused_key("", CONFIG).is_none());
        assert!(refused_key("error: the socket is in use\n", CONFIG).is_none());
        // The excerpt is not the line the MUD wrote there.
        assert!(
            refused_key(&good.replace("4 | [compaction]", "4 | [compactio]"), CONFIG).is_none()
        );
        // The field is not the key at that place.
        assert!(
            refused_key(
                &good.replace("field `compaction`", "field `memory`"),
                CONFIG
            )
            .is_none()
        );
        // The place holds no key.
        assert!(refused_key(&good.replace("column 2", "column 3"), CONFIG).is_none());
        assert!(refused_key(&good.replace("line 4,", "line 5,"), CONFIG).is_none());
        // Another error at the same place.
        assert!(
            refused_key(
                &good.replace(
                    "unknown field `compaction`, expected",
                    "invalid type: `compaction`, expected"
                ),
                CONFIG
            )
            .is_none()
        );
        // No carets, or a cut message.
        assert!(refused_key(&good.replace("^^^^^^^^^^", ""), CONFIG).is_none());
        let cut: String = good.lines().take(4).collect::<Vec<_>>().join("\n");
        assert!(refused_key(&cut, CONFIG).is_none());
        assert!(refused_key(&good.replace("line 4,", "line 04,"), CONFIG).is_none());
    }

    #[test]
    fn the_last_refusal_counts() {
        let first = old_lg_log(4, "[compaction]", " ^^^^^^^^^^", "compaction");
        let second = old_lg_log(9, "research = true", "^^^^^^^^", "research");
        assert_eq!(
            refused_key(&format!("{first}{second}"), CONFIG).as_deref(),
            Some("heartbeat.research")
        );
    }

    #[test]
    fn a_binary_lacks_a_part_once() {
        let bin = PathBuf::from("/nonexistent/compat-test/lg");
        assert!(lacks(&bin).is_empty());
        assert!(record(&bin, Optional::Compaction));
        assert!(!record(&bin, Optional::Compaction), "warned once");
        assert_eq!(lacks(&bin), BTreeSet::from([Optional::Compaction]));
        assert!(lacks(Path::new("/nonexistent/compat-test/other-lg")).is_empty());
    }

    #[test]
    fn a_bare_name_resolves_on_path() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_bin(&dir.path().join("lg")), dir.path().join("lg"));
        assert_eq!(
            resolve_bin(Path::new("no-such-lg-on-any-path")),
            PathBuf::from("no-such-lg-on-any-path")
        );
        assert!(resolve_bin(Path::new("sh")).is_absolute(), "sh is on PATH");
    }

    #[test]
    fn the_log_tail_is_its_last_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lg.log");
        assert_eq!(log_tail(&path), "", "no log");
        let long = format!("{}\nerror: the end\n", "x".repeat(40_000));
        std::fs::write(&path, &long).unwrap();
        let tail = log_tail(&path);
        assert!(tail.len() <= 16 * 1024 && tail.ends_with("error: the end\n"));
    }
}
