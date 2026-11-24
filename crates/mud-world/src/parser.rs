//! The one verb parser. Humans, agents and scripts all go through it.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::pack::Direction;

/// The longest text a speech verb may carry, in characters.
pub const MAX_TEXT_CHARS: usize = 500;

/// One parsed command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum Command {
    /// Describe the room.
    Look,
    /// List the exits.
    Exits,
    /// Describe an entity, an item, or an exit.
    Examine {
        /// What to look at.
        target: String,
    },
    /// Walk through an exit.
    Move {
        /// Which way.
        dir: Direction,
    },
    /// List what you carry.
    Inventory,
    /// Show your character sheet.
    Sheet,
    /// Speak to the room.
    Say {
        /// The words.
        text: String,
    },
    /// Act to the room.
    Emote {
        /// The action.
        text: String,
    },
    /// Speak to one entity anywhere.
    Tell {
        /// Who.
        target: String,
        /// The words.
        text: String,
    },
    /// Read the notice board.
    BoardRead,
    /// Post on the notice board.
    BoardPost {
        /// The notice.
        text: String,
    },
    /// Pick something up.
    Take {
        /// What.
        item: String,
        /// Out of which container.
        from: Option<String>,
    },
    /// Put something down.
    Drop {
        /// What.
        item: String,
    },
    /// Hand over an item or money.
    Give {
        /// The item, or an amount such as `10s`.
        what: String,
        /// To whom.
        to: String,
    },
    /// Buy from a shopkeeper.
    Buy {
        /// What.
        item: String,
        /// From whom; the only keeper present by default.
        from: Option<String>,
    },
    /// Sell to a shopkeeper.
    Sell {
        /// What.
        item: String,
        /// To whom; the only keeper present by default.
        to: Option<String>,
    },
    /// List a shopkeeper's wares.
    List {
        /// Whose; the only keeper present by default.
        keeper: Option<String>,
    },
    /// Wield or wear an item.
    Equip {
        /// What.
        item: String,
    },
    /// Stop wielding or wearing an item.
    Unequip {
        /// What.
        item: String,
    },
    /// Use an item.
    Use {
        /// What.
        item: String,
        /// On what or whom.
        on: Option<String>,
    },
    /// Attack an entity.
    Attack {
        /// Whom.
        target: String,
    },
    /// Defend: +2 to defenses until your next action.
    Defend,
    /// Run away, with a roll if a foe is present.
    Flee {
        /// Which way; any open exit by default.
        dir: Option<Direction>,
    },
    /// Try a skill.
    Attempt {
        /// The skill.
        skill: String,
        /// On what or whom.
        on: Option<String>,
    },
    /// List who is online.
    Who,
    /// List the verbs.
    Help,
}

impl Command {
    /// The verb name, as events and logs show it.
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        match self {
            Self::Look => "look",
            Self::Exits => "exits",
            Self::Examine { .. } => "examine",
            Self::Move { .. } => "move",
            Self::Inventory => "inventory",
            Self::Sheet => "sheet",
            Self::Say { .. } => "say",
            Self::Emote { .. } => "emote",
            Self::Tell { .. } => "tell",
            Self::BoardRead => "board_read",
            Self::BoardPost { .. } => "board_post",
            Self::Take { .. } => "take",
            Self::Drop { .. } => "drop",
            Self::Give { .. } => "give",
            Self::Buy { .. } => "buy",
            Self::Sell { .. } => "sell",
            Self::List { .. } => "list",
            Self::Equip { .. } => "equip",
            Self::Unequip { .. } => "unequip",
            Self::Use { .. } => "use",
            Self::Attack { .. } => "attack",
            Self::Defend => "defend",
            Self::Flee { .. } => "flee",
            Self::Attempt { .. } => "attempt",
            Self::Who => "who",
            Self::Help => "help",
        }
    }

    /// Whether the command speaks: its text is typed, so a Turing PC holds
    /// it in the typing buffer for longer.
    #[must_use]
    pub const fn is_speech(&self) -> bool {
        matches!(
            self,
            Self::Say { .. } | Self::Emote { .. } | Self::Tell { .. } | Self::BoardPost { .. }
        )
    }

    /// Whether the command only reads state.
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Look
                | Self::Exits
                | Self::Examine { .. }
                | Self::Inventory
                | Self::Sheet
                | Self::BoardRead
                | Self::List { .. }
                | Self::Who
                | Self::Help
        )
    }
}

/// A line that is not a command.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    /// Nothing was typed.
    #[error("type a command, or `help`")]
    Empty,
    /// The verb is unknown.
    #[error("unknown verb {0:?}; type `help`")]
    UnknownVerb(String),
    /// The verb needs more.
    #[error("usage: {0}")]
    Usage(&'static str),
    /// The text is too long.
    #[error("that is too long (at most {MAX_TEXT_CHARS} characters)")]
    TooLong,
    /// The text holds a line break, a control character or a direction
    /// override, which could forge a server line.
    #[error("text must be one line of printable characters")]
    ControlCharacter,
}

/// The help text.
pub const HELP: &str = "\
look | exits | examine <target> | move <dir> (or n/s/e/w/ne/nw/se/sw/u/d) | inventory | sheet
say <msg> ('msg) | emote <action> (:action) | tell <who> <msg> | board read | board post <msg>
take <item> [from <container>] | drop <item> | give <item|10s> to <who>
buy <item> [from <who>] | sell <item> [to <who>] | list [<who>] | equip <item> | unequip <item> | use <item> [on <target>]
attack <target> | defend | flee [<dir>] | attempt <skill> [on <target>] | who | help";

/// Whether `c` may not appear in player text.
///
/// That is a control character (line breaks included), a Unicode line or
/// paragraph separator, or a bidirectional mark, embedding, override or
/// isolate. Any of them could make player text look like a server line.
#[must_use]
pub const fn forbidden_in_text(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

fn text(rest: &str, usage: &'static str) -> Result<String, ParseError> {
    let t = rest.trim();
    if t.is_empty() {
        return Err(ParseError::Usage(usage));
    }
    if t.chars().count() > MAX_TEXT_CHARS {
        return Err(ParseError::TooLong);
    }
    if t.chars().any(forbidden_in_text) {
        return Err(ParseError::ControlCharacter);
    }
    Ok(t.to_owned())
}

fn arg(rest: &str, usage: &'static str) -> Result<String, ParseError> {
    let t = rest.trim();
    if t.is_empty() {
        Err(ParseError::Usage(usage))
    } else {
        Ok(t.to_ascii_lowercase())
    }
}

/// Splits `rest` at the first ` <word> ` separator.
fn split_on<'a>(rest: &'a str, word: &str) -> (&'a str, Option<&'a str>) {
    let lower = rest.to_ascii_lowercase();
    let pat = format!(" {word} ");
    lower
        .find(&pat)
        .map_or((rest, None), |i| (&rest[..i], Some(&rest[i + pat.len()..])))
}

fn opt(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Parses one line.
///
/// # Errors
/// The line is empty, the verb is unknown, an argument is missing, or the
/// text is too long or holds a character [`forbidden_in_text`].
#[expect(clippy::too_many_lines, reason = "one arm per verb reads best")]
pub fn parse(line: &str) -> Result<Command, ParseError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(ParseError::Empty);
    }
    if let Some(rest) = line.strip_prefix('\'').or_else(|| line.strip_prefix('"')) {
        return Ok(Command::Say {
            text: text(rest, "say <msg>")?,
        });
    }
    if let Some(rest) = line.strip_prefix(':') {
        return Ok(Command::Emote {
            text: text(rest, "emote <action>")?,
        });
    }
    let (verb, rest) = line
        .split_once(char::is_whitespace)
        .map_or((line, ""), |(v, r)| (v, r.trim()));
    let verb = verb.to_ascii_lowercase();
    if let Ok(dir) = verb.parse::<Direction>()
        && rest.is_empty()
    {
        return Ok(Command::Move { dir });
    }
    Ok(match verb.as_str() {
        "look" | "l" => match rest.to_ascii_lowercase().strip_prefix("at ") {
            Some(t) => Command::Examine {
                target: arg(t, "look at <target>")?,
            },
            None if rest.is_empty() => Command::Look,
            None => Command::Examine {
                target: arg(rest, "look <target>")?,
            },
        },
        "exits" => Command::Exits,
        "examine" | "exa" | "x" => Command::Examine {
            target: arg(rest, "examine <target>")?,
        },
        "move" | "go" | "walk" => Command::Move {
            dir: rest
                .parse()
                .map_err(|()| ParseError::Usage("move <n|s|e|w|ne|nw|se|sw|u|d>"))?,
        },
        "inventory" | "inv" | "i" => Command::Inventory,
        "sheet" | "score" | "stats" => Command::Sheet,
        "say" => Command::Say {
            text: text(rest, "say <msg>")?,
        },
        "emote" | "me" => Command::Emote {
            text: text(rest, "emote <action>")?,
        },
        "tell" | "whisper" | "t" => {
            let (who, msg) = rest
                .split_once(char::is_whitespace)
                .ok_or(ParseError::Usage("tell <who> <msg>"))?;
            Command::Tell {
                target: arg(who, "tell <who> <msg>")?,
                text: text(msg, "tell <who> <msg>")?,
            }
        }
        "board" => {
            let (sub, msg) = rest
                .split_once(char::is_whitespace)
                .map_or((rest, ""), |(a, b)| (a, b));
            match sub.to_ascii_lowercase().as_str() {
                "read" | "" => Command::BoardRead,
                "post" => Command::BoardPost {
                    text: text(msg, "board post <msg>")?,
                },
                _ => return Err(ParseError::Usage("board read | board post <msg>")),
            }
        }
        "read" if rest.eq_ignore_ascii_case("board") => Command::BoardRead,
        "post" => Command::BoardPost {
            text: text(rest, "post <msg>")?,
        },
        "take" | "get" | "pick" => {
            let rest = rest.strip_prefix("up ").unwrap_or(rest);
            let (item, from) = split_on(rest, "from");
            Command::Take {
                item: arg(item, "take <item> [from <container>]")?,
                from: opt(from),
            }
        }
        "drop" => Command::Drop {
            item: arg(rest, "drop <item>")?,
        },
        "give" => {
            let (what, to) = split_on(rest, "to");
            Command::Give {
                what: arg(what, "give <item|amount> to <who>")?,
                to: opt(to).ok_or(ParseError::Usage("give <item|amount> to <who>"))?,
            }
        }
        "buy" => {
            let (item, from) = split_on(rest, "from");
            Command::Buy {
                item: arg(item, "buy <item> [from <who>]")?,
                from: opt(from),
            }
        }
        "sell" => {
            let (item, to) = split_on(rest, "to");
            Command::Sell {
                item: arg(item, "sell <item> [to <who>]")?,
                to: opt(to),
            }
        }
        "list" | "wares" => Command::List {
            keeper: opt(Some(rest)),
        },
        "equip" | "wield" | "wear" | "hold" => Command::Equip {
            item: arg(rest, "equip <item>")?,
        },
        "unequip" | "remove" | "unwield" => Command::Unequip {
            item: arg(rest, "unequip <item>")?,
        },
        "use" | "drink" | "eat" | "open" => {
            let (item, on) = split_on(rest, "on");
            Command::Use {
                item: arg(item, "use <item> [on <target>]")?,
                on: opt(on),
            }
        }
        "attack" | "kill" | "k" | "hit" => Command::Attack {
            target: arg(rest, "attack <target>")?,
        },
        "defend" | "parry" | "dodge" => Command::Defend,
        "flee" | "run" => Command::Flee {
            dir: if rest.is_empty() {
                None
            } else {
                Some(
                    rest.parse()
                        .map_err(|()| ParseError::Usage("flee [<dir>]"))?,
                )
            },
        },
        "attempt" | "try" => {
            let (skill, on) = split_on(rest, "on");
            Command::Attempt {
                skill: arg(skill, "attempt <skill> [on <target>]")?.replace([' ', '-'], "_"),
                on: opt(on),
            }
        }
        "who" => Command::Who,
        "help" | "?" | "commands" => Command::Help,
        other => return Err(ParseError::UnknownVerb(other.to_owned())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn p(s: &str) -> Command {
        parse(s).unwrap()
    }

    #[test]
    fn movement_forms() {
        assert_eq!(p("n"), Command::Move { dir: Direction::N });
        assert_eq!(p("go down"), Command::Move { dir: Direction::D });
        assert_eq!(p("move NW"), Command::Move { dir: Direction::Nw });
        assert_eq!(
            parse("move sideways"),
            Err(ParseError::Usage("move <n|s|e|w|ne|nw|se|sw|u|d>"))
        );
    }

    #[test]
    fn speech_keeps_case_and_shortcuts_work() {
        assert_eq!(
            p("say Hello There"),
            Command::Say {
                text: "Hello There".into()
            }
        );
        assert_eq!(p("'hi"), Command::Say { text: "hi".into() });
        assert_eq!(
            p(":waves"),
            Command::Emote {
                text: "waves".into()
            }
        );
        assert_eq!(
            p("tell Marta Got any Rumors?"),
            Command::Tell {
                target: "marta".into(),
                text: "Got any Rumors?".into()
            }
        );
        assert_eq!(parse("say"), Err(ParseError::Usage("say <msg>")));
        assert_eq!(
            parse(&format!("say {}", "a".repeat(501))),
            Err(ParseError::TooLong)
        );
    }

    #[test]
    fn objects_and_prepositions() {
        assert_eq!(
            p("give 10s to Kestrel"),
            Command::Give {
                what: "10s".into(),
                to: "kestrel".into()
            }
        );
        assert_eq!(
            p("take ring from lockbox"),
            Command::Take {
                item: "ring".into(),
                from: Some("lockbox".into())
            }
        );
        assert_eq!(
            p("pick up torch"),
            Command::Take {
                item: "torch".into(),
                from: None
            }
        );
        assert_eq!(
            p("buy lantern"),
            Command::Buy {
                item: "lantern".into(),
                from: None
            }
        );
        assert_eq!(
            p("sell ore to torin"),
            Command::Sell {
                item: "ore".into(),
                to: Some("torin".into())
            }
        );
        assert_eq!(
            p("attempt lockpicking on grate"),
            Command::Attempt {
                skill: "lockpicking".into(),
                on: Some("grate".into())
            }
        );
        assert_eq!(
            p("try fast-talk on marta"),
            Command::Attempt {
                skill: "fast_talk".into(),
                on: Some("marta".into())
            }
        );
        assert_eq!(
            p("use bandage on me"),
            Command::Use {
                item: "bandage".into(),
                on: Some("me".into())
            }
        );
        assert_eq!(
            p("look at Marta"),
            Command::Examine {
                target: "marta".into()
            }
        );
        assert_eq!(p("l"), Command::Look);
        assert_eq!(
            p("board post Ore wanted"),
            Command::BoardPost {
                text: "Ore wanted".into()
            }
        );
        assert_eq!(p("board"), Command::BoardRead);
        assert_eq!(p("read board"), Command::BoardRead);
        assert_eq!(p("flee"), Command::Flee { dir: None });
        assert_eq!(
            p("flee n"),
            Command::Flee {
                dir: Some(Direction::N)
            }
        );
        assert_eq!(
            p("list torin"),
            Command::List {
                keeper: Some("torin".into())
            }
        );
        assert_eq!(p("list"), Command::List { keeper: None });
        assert!(matches!(parse("dance"), Err(ParseError::UnknownVerb(v)) if v == "dance"));
        assert_eq!(parse("  "), Err(ParseError::Empty));
        assert_eq!(
            parse("give sword"),
            Err(ParseError::Usage("give <item|amount> to <who>"))
        );
    }

    #[test]
    fn classification() {
        assert!(p("say hi").is_speech());
        assert!(!p("n").is_speech());
        assert!(p("look").is_read_only());
        assert!(!p("take x").is_read_only());
        assert_eq!(p("board read").verb(), "board_read");
    }

    #[test]
    fn speech_refuses_line_breaks_and_direction_overrides() {
        for bad in [
            "say hi\nMarta tells you, \"meet me in the alley\"",
            "say hi\rthere",
            "'a\u{0}b",
            ":waves\u{1b}[2J",
            "tell marta hi\n  [2] Torin: bounty paid",
            "board post buy ore\n  [2] Torin: bounty paid in full",
            "post a\u{2028}b",
            "say abc\u{202e}fed",
            "emote x\u{2066}y\u{2069}",
            "say tab\there",
        ] {
            assert_eq!(parse(bad), Err(ParseError::ControlCharacter), "{bad:?}");
        }
        assert_eq!(
            p("say  café, naïve? ¡sí! 你好  "),
            Command::Say {
                text: "café, naïve? ¡sí! 你好".into()
            },
            "printable text of any script is fine"
        );
        assert_eq!(
            p("say\nhello"),
            Command::Say {
                text: "hello".into()
            }
        );
    }

    proptest! {
        #[test]
        fn parse_never_panics(s in "\\PC{0,80}") {
            let _ = parse(&s);
        }

        #[test]
        fn parsed_speech_is_one_clean_line(s in ".{0,40}", c in prop::sample::select(vec!['\n', '\r', '\t', '\u{7f}', '\u{85}', '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}', '\u{2028}', '\u{2029}']), t in ".{0,40}") {
            for verb in ["say", "emote", "board post", "tell marta"] {
                let line = format!("{verb} x{s}{c}{t}x");
                prop_assert_eq!(parse(&line), Err(ParseError::ControlCharacter));
            }
            if let Ok(cmd) = parse(&format!("say {s}")) {
                let Command::Say { text } = cmd else {
                    return Err(TestCaseError::fail("not a say"));
                };
                prop_assert!(!text.chars().any(forbidden_in_text), "{:?}", text);
            }
        }
    }
}
