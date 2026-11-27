//! The script API: the curated natives, the effect constructors, the dice
//! and the log. `docs/scripting.md` lists exactly this surface.
//!
//! Every native whose cost grows with its input charges the call's work
//! counter (see [`crate::sandbox::charge`]), so no single operation can
//! do unbounded work.

use mud_rules::{DamageDice, DamageType, DiceRng as _, EFFECTIVE_LIMIT, Outcome, success_roll};
use std::collections::BTreeMap;

use mud_world::hooks::{FlagValue, ScriptEffect, USE_UP_MAX};
use mud_world::pack::Direction;
use mud_world::rewards::REWARD_ITEMS_MAX;
use mud_world::{PoolId, ProtoId};
use rhai::{Array, Dynamic, Engine, Map, NativeCallContext};

use crate::sandbox::{
    CallKind, LOG_LINE_MAX, LOG_LINES_MAX, MAX_ARRAY_SIZE, RhaiResult, charge, fault, with_state,
};

/// One effect a world hook returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect(pub ScriptEffect);

/// The most dice a script may roll in one expression.
pub const SCRIPT_DICE_MAX: u8 = 6;

fn len_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Parses dice for a script: the pack syntax, 1 to 6 dice.
///
/// # Errors
/// Anything else.
pub fn script_dice(s: &str) -> Result<DamageDice, String> {
    s.parse::<DamageDice>()
        .ok()
        .filter(|d| (1..=SCRIPT_DICE_MAX).contains(&d.dice))
        .ok_or_else(|| format!("dice {s:?}: scripts use 1d to 6d with adds"))
}

fn hook_only(ctx: &NativeCallContext, name: &str, fx: ScriptEffect) -> RhaiResult<Effect> {
    with_state(ctx, |s| {
        if s.kind == CallKind::Hook {
            Ok(Effect(fx))
        } else {
            Err(format!(
                "{name}() is for room, item and verb scripts; a mind script returns command lines"
            ))
        }
    })
}

fn direction(name: &str, d: &str) -> RhaiResult<Direction> {
    d.parse::<Direction>()
        .map_err(|()| fault(format!("{name}({d:?}): not a direction")))
}

/// The ask of `reward(pool, #{ currency, items })`: only those two keys,
/// a currency of zero or more, and items as a map of prototype to a count
/// of 1 to 16. The engine checks the pool, the prototypes and the budget.
fn reward_ask(ask: &Map) -> RhaiResult<(i64, BTreeMap<ProtoId, u32>)> {
    let mut currency = 0;
    let mut items = BTreeMap::new();
    for (k, v) in ask {
        match k.as_str() {
            "currency" => {
                currency = v
                    .as_int()
                    .ok()
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| fault("reward(): currency is an integer of 0 or more"))?;
            }
            "items" => {
                let m = v
                    .clone()
                    .try_cast::<Map>()
                    .ok_or_else(|| fault("reward(): items is a map of item id to count"))?;
                for (p, n) in m {
                    let n = n
                        .as_int()
                        .ok()
                        .and_then(|n| u32::try_from(n).ok())
                        .filter(|n| (1..=REWARD_ITEMS_MAX).contains(n))
                        .ok_or_else(|| {
                            fault(format!(
                                "reward(): items.{p} is a count of 1 to {REWARD_ITEMS_MAX}"
                            ))
                        })?;
                    items.insert(ProtoId::new(p.as_str()), n);
                }
            }
            other => {
                return Err(fault(format!(
                    "reward(): unknown key {other:?}; use currency and items"
                )));
            }
        }
    }
    Ok((currency, items))
}

fn register_rewards(e: &mut Engine) {
    e.register_fn(
        "reward",
        |ctx: NativeCallContext, pool: String, ask: Map| -> RhaiResult<Effect> {
            let (currency, items) = reward_ask(&ask)?;
            hook_only(
                &ctx,
                "reward",
                ScriptEffect::Reward {
                    pool: PoolId::new(pool),
                    currency,
                    items,
                },
            )
        },
    );
    e.register_fn(
        "reward",
        |ctx: NativeCallContext, pool: String, currency: i64| -> RhaiResult<Effect> {
            if currency < 0 {
                return Err(fault("reward(): currency is an integer of 0 or more"));
            }
            hook_only(
                &ctx,
                "reward",
                ScriptEffect::Reward {
                    pool: PoolId::new(pool),
                    currency,
                    items: BTreeMap::new(),
                },
            )
        },
    );
}

/// `use_up(proto, n)`: n is a count of 1 to [`USE_UP_MAX`]. The engine
/// checks the prototype and what the actor carries.
fn use_up(ctx: &NativeCallContext, proto: String, n: i64) -> RhaiResult<Effect> {
    let count = u32::try_from(n)
        .ok()
        .filter(|n| (1..=USE_UP_MAX).contains(n))
        .ok_or_else(|| fault(format!("use_up(): n is a count of 1 to {USE_UP_MAX}")))?;
    hook_only(
        ctx,
        "use_up",
        ScriptEffect::UseUp {
            proto: ProtoId::new(proto),
            count,
        },
    )
}

fn register_effects(e: &mut Engine) {
    e.register_type_with_name::<Effect>("Effect");
    e.register_fn("to_string", |fx: &mut Effect| fx.0.describe());
    e.register_fn("to_debug", |fx: &mut Effect| fx.0.describe());
    e.register_fn("msg", |ctx: NativeCallContext, t: String| {
        hook_only(&ctx, "msg", ScriptEffect::Msg(t))
    });
    e.register_fn("narrate", |ctx: NativeCallContext, t: String| {
        hook_only(&ctx, "narrate", ScriptEffect::Narrate(t))
    });
    e.register_fn("refuse", |ctx: NativeCallContext, t: String| {
        hook_only(&ctx, "refuse", ScriptEffect::Refuse(t))
    });
    e.register_fn("unlock", |ctx: NativeCallContext, d: &str| {
        let d = direction("unlock", d)?;
        hook_only(&ctx, "unlock", ScriptEffect::Unlock(d))
    });
    e.register_fn("lock", |ctx: NativeCallContext, d: &str| {
        let d = direction("lock", d)?;
        hook_only(&ctx, "lock", ScriptEffect::Lock(d))
    });
    e.register_fn("set_flag", |ctx: NativeCallContext, n: String, v: bool| {
        hook_only(
            &ctx,
            "set_flag",
            ScriptEffect::SetFlag(n, FlagValue::Bool(v)),
        )
    });
    e.register_fn("set_flag", |ctx: NativeCallContext, n: String, v: i64| {
        hook_only(
            &ctx,
            "set_flag",
            ScriptEffect::SetFlag(n, FlagValue::Int(v)),
        )
    });
    e.register_fn(
        "set_flag",
        |ctx: NativeCallContext, n: String, v: String| {
            hook_only(
                &ctx,
                "set_flag",
                ScriptEffect::SetFlag(n, FlagValue::Text(v)),
            )
        },
    );
    e.register_fn("mark", |ctx: NativeCallContext, n: String, v: bool| {
        hook_only(&ctx, "mark", ScriptEffect::Mark(n, FlagValue::Bool(v)))
    });
    e.register_fn("mark", |ctx: NativeCallContext, n: String, v: i64| {
        hook_only(&ctx, "mark", ScriptEffect::Mark(n, FlagValue::Int(v)))
    });
    e.register_fn("mark", |ctx: NativeCallContext, n: String, v: String| {
        hook_only(&ctx, "mark", ScriptEffect::Mark(n, FlagValue::Text(v)))
    });
    e.register_fn("clear_flag", |ctx: NativeCallContext, n: String| {
        hook_only(&ctx, "clear_flag", ScriptEffect::ClearFlag(n))
    });
    e.register_fn("unmark", |ctx: NativeCallContext, n: String| {
        hook_only(&ctx, "unmark", ScriptEffect::Unmark(n))
    });
    e.register_fn("use_up", |ctx: NativeCallContext, p: String| {
        use_up(&ctx, p, 1)
    });
    e.register_fn("use_up", |ctx: NativeCallContext, p: String, n: i64| {
        use_up(&ctx, p, n)
    });
    e.register_fn("heal", |ctx: NativeCallContext, d: &str| {
        let d = script_dice(d).map_err(fault)?;
        hook_only(&ctx, "heal", ScriptEffect::Heal(d))
    });
    e.register_fn("hurt", |ctx: NativeCallContext, d: &str| {
        let d = script_dice(d).map_err(fault)?;
        hook_only(&ctx, "hurt", ScriptEffect::Hurt(d))
    });
}

fn register_dice(e: &mut Engine) {
    e.register_fn("d6", |ctx: NativeCallContext| -> RhaiResult<i64> {
        with_state(&ctx, |s| Ok(i64::from(s.dice.d6())))
    });
    e.register_fn(
        "roll",
        |ctx: NativeCallContext, d: &str| -> RhaiResult<i64> {
            let d = script_dice(d).map_err(fault)?;
            with_state(&ctx, |s| {
                Ok(i64::from(d.roll(&mut s.dice, DamageType::Crushing).max(0)))
            })
        },
    );
    e.register_fn(
        "below",
        |ctx: NativeCallContext, n: i64| -> RhaiResult<i64> {
            let n = u32::try_from(n)
                .ok()
                .filter(|n| (1..=1_000_000).contains(n))
                .ok_or_else(|| fault(format!("below({n}): n is 1 to 1000000")))?;
            with_state(&ctx, |s| Ok(i64::from(s.dice.below(n))))
        },
    );
    e.register_fn(
        "chance",
        |ctx: NativeCallContext, pct: i64| -> RhaiResult<bool> {
            if !(0..=100).contains(&pct) {
                return Err(fault(format!("chance({pct}): pct is 0 to 100")));
            }
            with_state(&ctx, |s| Ok(i64::from(s.dice.below(100)) < pct))
        },
    );
    e.register_fn(
        "check",
        |ctx: NativeCallContext, level: i64| -> RhaiResult<Map> {
            let clamped = level.clamp(i64::from(-EFFECTIVE_LIMIT), i64::from(EFFECTIVE_LIMIT));
            let level = i32::try_from(clamped).unwrap_or(0);
            with_state(&ctx, |s| {
                let r = success_roll(&mut s.dice, level);
                let mut m = Map::new();
                m.insert("success".into(), Dynamic::from_bool(r.outcome.is_success()));
                m.insert(
                    "critical".into(),
                    Dynamic::from_bool(matches!(
                        r.outcome,
                        Outcome::CriticalSuccess | Outcome::CriticalFailure
                    )),
                );
                m.insert("margin".into(), Dynamic::from_int(i64::from(r.margin)));
                m.insert("total".into(), Dynamic::from_int(i64::from(r.total)));
                Ok(m)
            })
        },
    );
}

fn register_log_and_time(e: &mut Engine) {
    e.register_fn(
        "log",
        |ctx: NativeCallContext, line: String| -> RhaiResult<()> {
            with_state(&ctx, |s| {
                if s.log.len() < LOG_LINES_MAX {
                    s.log.push(line.chars().take(LOG_LINE_MAX).collect());
                } else {
                    s.log_dropped += 1;
                }
                Ok(())
            })
        },
    );
    e.register_fn("hour_utc", |ms: i64| ms.rem_euclid(86_400_000) / 3_600_000);
    e.register_fn("minute_of_day_utc", |ms: i64| {
        ms.rem_euclid(86_400_000) / 60_000
    });
    e.register_fn("day_utc", |ms: i64| ms.div_euclid(86_400_000));
}

fn same(a: &Dynamic, b: &Dynamic) -> bool {
    if let (Ok(x), Ok(y)) = (a.as_int(), b.as_int()) {
        return x == y;
    }
    if let (Ok(x), Ok(y)) = (a.as_bool(), b.as_bool()) {
        return x == y;
    }
    if a.is_string() && b.is_string() {
        return a.clone().into_string().ok() == b.clone().into_string().ok();
    }
    false
}

/// An array a native grows in place is checked here: Rhai checks the
/// sizes of values it builds, not of arrays a native mutates.
fn fits(len: usize) -> RhaiResult<()> {
    if len > MAX_ARRAY_SIZE {
        Err(fault("Size of array/BLOB too large"))
    } else {
        Ok(())
    }
}

fn register_arrays(e: &mut Engine) {
    e.register_fn("len", |a: &mut Array| len_i64(a.len()));
    e.register_fn("is_empty", |a: &mut Array| a.is_empty());
    e.register_fn(
        "push",
        |ctx: NativeCallContext, a: &mut Array, v: Dynamic| -> RhaiResult<()> {
            charge(&ctx, 1)?;
            fits(a.len() + 1)?;
            a.push(v);
            Ok(())
        },
    );
    e.register_fn("pop", |a: &mut Array| a.pop().unwrap_or(Dynamic::UNIT));
    e.register_fn(
        "+=",
        |ctx: NativeCallContext, a: &mut Array, b: Array| -> RhaiResult<()> {
            charge(&ctx, b.len())?;
            fits(a.len() + b.len())?;
            a.extend(b);
            Ok(())
        },
    );
    e.register_fn(
        "+",
        |ctx: NativeCallContext, a: Array, b: Array| -> RhaiResult<Array> {
            charge(&ctx, a.len() + b.len())?;
            fits(a.len() + b.len())?;
            let mut out = a;
            out.extend(b);
            Ok(out)
        },
    );
    e.register_fn(
        "contains",
        |ctx: NativeCallContext, a: &mut Array, x: Dynamic| -> RhaiResult<bool> {
            charge(&ctx, a.len())?;
            Ok(a.iter().any(|y| same(y, &x)))
        },
    );
    e.register_iterator::<Array>();
}

fn register_maps(e: &mut Engine) {
    e.register_fn("len", |m: &mut Map| len_i64(m.len()));
    e.register_fn("is_empty", |m: &mut Map| m.is_empty());
    e.register_fn("contains", |m: &mut Map, k: &str| m.contains_key(k));
    e.register_fn(
        "keys",
        |ctx: NativeCallContext, m: &mut Map| -> RhaiResult<Array> {
            charge(&ctx, m.len())?;
            Ok(m.keys().map(|k| Dynamic::from(k.to_string())).collect())
        },
    );
    e.register_fn("remove", |m: &mut Map, k: &str| {
        m.remove(k).unwrap_or(Dynamic::UNIT)
    });
}

fn register_strings(e: &mut Engine) {
    e.register_fn("len", |s: &str| len_i64(s.chars().count()));
    e.register_fn("is_empty", |s: &str| s.is_empty());
    e.register_fn("to_lower", |s: &str| s.to_lowercase());
    e.register_fn("to_upper", |s: &str| s.to_uppercase());
    e.register_fn("trim", |s: &str| s.trim().to_owned());
    e.register_fn(
        "starts_with",
        |ctx: NativeCallContext, s: &str, p: &str| -> RhaiResult<bool> {
            charge(&ctx, s.len())?;
            Ok(s.starts_with(p))
        },
    );
    e.register_fn(
        "ends_with",
        |ctx: NativeCallContext, s: &str, p: &str| -> RhaiResult<bool> {
            charge(&ctx, s.len())?;
            Ok(s.ends_with(p))
        },
    );
    e.register_fn(
        "contains",
        |ctx: NativeCallContext, s: &str, p: &str| -> RhaiResult<bool> {
            charge(&ctx, s.len())?;
            Ok(s.contains(p))
        },
    );
    e.register_fn("sub_string", |s: &str, start: i64, len: i64| {
        let skip = usize::try_from(start.max(0)).unwrap_or(usize::MAX);
        let take = usize::try_from(len.max(0)).unwrap_or(0);
        s.chars().skip(skip).take(take).collect::<String>()
    });
}

/// Registers the whole script API.
pub fn register(e: &mut Engine) {
    register_arrays(e);
    register_maps(e);
    register_strings(e);
    register_effects(e);
    register_rewards(e);
    register_dice(e);
    register_log_and_time(e);
    crate::view::register(e);
    crate::untrusted::register(e);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_dice_are_one_to_six() {
        assert_eq!(script_dice("1d").unwrap(), DamageDice::new(1, 0));
        assert_eq!(script_dice("6d+2").unwrap(), DamageDice::new(6, 2));
        assert_eq!(script_dice("1d-2").unwrap(), DamageDice::new(1, -2));
        for bad in ["7d", "0d", "x", "20d", "1d+x"] {
            assert_eq!(
                script_dice(bad).unwrap_err(),
                format!("dice {bad:?}: scripts use 1d to 6d with adds")
            );
        }
        assert!(same(
            &Dynamic::from("a".to_owned()),
            &Dynamic::from("a".to_owned())
        ));
        assert!(!same(&Dynamic::from_int(1), &Dynamic::from_bool(true)));
    }
}
