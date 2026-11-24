//! `Untrusted`: player text as a script sees it.
//!
//! A script cannot quote player text. It can learn only whether each of at
//! most [`PROBE_CAP`] dictionary words of its own choosing (written in the
//! pack) appears in one text, per call. Turning it into a string gives
//! `[player text]`; joining it to a string, comparing it, or passing it to
//! an effect, a log line, a command line or memory is a fault or "function
//! not found".

use std::fmt;
use std::sync::Arc;

use rhai::{Array, Dynamic, Engine, NativeCallContext};

use crate::sandbox::{RhaiResult, fault, with_state};

/// The most search words one text may be tested against in one call.
pub const PROBE_CAP: u32 = 16;

/// What `to_string` shows instead of the text.
pub const PLACEHOLDER: &str = "[player text]";

/// The message of the blocked operations.
pub const NO_QUOTING: &str = "player text cannot be quoted or compared; test it with has_word()";

/// Player text, opaque to scripts.
#[derive(Clone)]
pub struct Untrusted {
    id: u32,
    text: Arc<str>,
}

impl fmt::Debug for Untrusted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Untrusted(..)")
    }
}

impl Untrusted {
    /// Wraps `text`; `id` must be fresh within one call.
    #[must_use]
    pub fn new(id: u32, text: &str) -> Self {
        Self {
            id,
            text: Arc::from(text),
        }
    }

    /// The words of the text, lowercase. A word is a run of letters,
    /// digits and apostrophes.
    fn words(&self) -> impl Iterator<Item = String> + '_ {
        self.text
            .split(|c: char| !(c.is_alphanumeric() || c == '\''))
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
    }

    fn contains_word(&self, w: &str) -> bool {
        self.words().any(|x| x == w)
    }
}

/// Checks a search word: 2 to 32 of `[a-z0-9']`.
fn search_word(w: &str) -> Result<(), String> {
    let ok = (2..=32).contains(&w.chars().count())
        && w.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '\'');
    if ok {
        Ok(())
    } else {
        Err("a search word is 2 to 32 lowercase letters, digits or apostrophes".to_owned())
    }
}

/// Counts `n` probes on `t`, failing past the cap.
fn probe(ctx: &NativeCallContext, t: &Untrusted, n: usize) -> RhaiResult<()> {
    with_state(ctx, |s| {
        let used = s.probes.entry(t.id).or_insert(0);
        *used = used.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        if *used > PROBE_CAP {
            Err(format!(
                "player text was probed more than {PROBE_CAP} times in one call"
            ))
        } else {
            Ok(())
        }
    })
}

fn words_of(list: &Array) -> RhaiResult<Vec<String>> {
    list.iter()
        .map(|d| {
            let w = d
                .clone()
                .into_string()
                .map_err(|_| fault("a search word must be a string"))?;
            search_word(&w).map_err(fault)?;
            Ok(w)
        })
        .collect()
}

/// Registers `Untrusted` and its few methods.
pub fn register(engine: &mut Engine) {
    engine.register_type_with_name::<Untrusted>("Untrusted");
    engine.register_fn(
        "has_word",
        |ctx: NativeCallContext, t: &mut Untrusted, w: &str| -> RhaiResult<bool> {
            search_word(w).map_err(fault)?;
            probe(&ctx, t, 1)?;
            Ok(t.contains_word(w))
        },
    );
    engine.register_fn(
        "has_any",
        |ctx: NativeCallContext, t: &mut Untrusted, list: Array| -> RhaiResult<bool> {
            let words = words_of(&list)?;
            probe(&ctx, t, words.len())?;
            Ok(words.iter().any(|w| t.contains_word(w)))
        },
    );
    engine.register_fn(
        "pick",
        |ctx: NativeCallContext, t: &mut Untrusted, list: Array| -> RhaiResult<Dynamic> {
            let words = words_of(&list)?;
            probe(&ctx, t, words.len())?;
            // The script's own string, never the player's.
            Ok(words
                .into_iter()
                .find(|w| t.contains_word(w))
                .map_or(Dynamic::UNIT, Dynamic::from))
        },
    );
    engine.register_fn("is_empty", |t: &mut Untrusted| t.text.trim().is_empty());
    engine.register_fn("to_string", |_: &mut Untrusted| PLACEHOLDER.to_owned());
    engine.register_fn("to_debug", |_: &mut Untrusted| PLACEHOLDER.to_owned());
    let refuse2 = |_: Untrusted, _: &str| -> RhaiResult<bool> { Err(fault(NO_QUOTING)) };
    let refuse2r = |_: &str, _: Untrusted| -> RhaiResult<bool> { Err(fault(NO_QUOTING)) };
    for op in ["==", "!="] {
        engine.register_fn(op, refuse2);
        engine.register_fn(op, refuse2r);
        engine.register_fn(op, |_: Untrusted, _: Untrusted| -> RhaiResult<bool> {
            Err(fault(NO_QUOTING))
        });
    }
    engine.register_fn("+", |_: Untrusted, _: &str| -> RhaiResult<String> {
        Err(fault(NO_QUOTING))
    });
    engine.register_fn("+", |_: &str, _: Untrusted| -> RhaiResult<String> {
        Err(fault(NO_QUOTING))
    });
    engine.register_fn("+=", |_: &mut String, _: Untrusted| -> RhaiResult<()> {
        Err(fault(NO_QUOTING))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_split_on_anything_but_letters_digits_and_apostrophes() {
        let t = Untrusted::new(0, "Vich?! the FENCE's don't-know 42");
        let w: Vec<String> = t.words().collect();
        assert_eq!(w, ["vich", "the", "fence's", "don't", "know", "42"]);
        assert!(t.contains_word("vich"));
        assert!(!t.contains_word("vic"));
        assert_eq!(format!("{t:?}"), "Untrusted(..)");
        assert!(search_word("a").is_err());
        assert!(search_word("ab c").is_err());
        assert!(search_word(&"a".repeat(33)).is_err());
        assert!(search_word("don't").is_ok());
    }
}
