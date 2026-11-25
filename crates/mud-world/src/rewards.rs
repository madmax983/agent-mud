//! Ledgered rewards: pack pools that world scripts may mint from (ADR 0010).
//!
//! A pool is physics, so it lives in the world pack (`economy.toml`
//! `[rewards.<pool>]`): a budget of currency and of item prototypes per
//! period, a UTC day of the game clock or ever. A hook asks for a reward
//! with `ScriptEffect::Reward`; the engine admits the ask whole and only
//! within what is left of the period, and records every mint in the
//! ledger: a [`MintRecord`] the store appends to its `mints` table with
//! the command's events, while the snapshot keeps only bounded counts
//! (`WorldState.pool_spent` and the per-pool [`MintTotals`]). Money and
//! items are conserved except for ledgered mints.
//!
//! Three kernels here are mirrored in `formal/verus/src/rewards.rs`:
//! [`pool_period`], [`pool_roll`] and [`pool_admit`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::{EntityId, ItemId, PoolId, ProtoId, Seq};

/// The most currency a pool may give per period.
pub const POOL_CURRENCY_MAX: i64 = 1_000_000;
/// The most items of one prototype a pool may give per period.
pub const POOL_ITEMS_MAX: u32 = 1_000;
/// The most items one reward may make.
pub const REWARD_ITEMS_MAX: u32 = 16;
/// The longest pool id.
pub const POOL_ID_MAX: usize = 32;

// mirror-begin pool_period
/// One day of the game clock, in milliseconds.
pub const DAY_MS: u64 = 86_400_000;

/// The period of a pool at game time `now_ms`: the UTC day for a per-day
/// pool, else 0 (a pool that never refills).
#[must_use]
pub const fn pool_period(per_day: bool, now_ms: u64) -> u64 {
    if per_day { now_ms / DAY_MS } else { 0 }
}
// mirror-end pool_period

// mirror-begin pool_roll
/// The period a pool counts in now, and what it has spent there.
///
/// It takes the period of the pool's last mint, what it had spent then, and
/// the period of now. A later period starts from zero; an earlier one (a
/// clock set back) keeps the later count, so setting the clock back never
/// refills a pool.
#[must_use]
pub const fn pool_roll(last: u64, spent: u64, period: u64) -> (u64, u64) {
    if period > last {
        (period, 0)
    } else {
        (last, spent)
    }
}
// mirror-end pool_roll

// mirror-begin pool_admit
/// Admits one ask against a pool's budget for the period: whether it fits
/// in what is left, and what the pool has spent after it. Never partly,
/// never past the budget, and never an overflow.
#[must_use]
pub const fn pool_admit(spent: u64, budget: u64, ask: u64) -> (bool, u64) {
    if spent <= budget && ask <= budget - spent {
        (true, spent + ask)
    } else {
        (false, spent)
    }
}
// mirror-end pool_admit

/// How often a pool refills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewardPeriod {
    /// Every UTC day of the game clock.
    Day,
    /// Never: the budget is all there will ever be.
    Ever,
}

impl RewardPeriod {
    /// `day` or `ever`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Ever => "ever",
        }
    }
}

/// A pool of rewards, as `economy.toml` declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewardPool {
    /// The currency it may give per period.
    #[serde(default)]
    pub currency: i64,
    /// The items it may make per period, by prototype.
    #[serde(default)]
    pub items: BTreeMap<ProtoId, u32>,
    /// How often it refills.
    pub per: RewardPeriod,
}

impl RewardPool {
    /// The period of game time `now_ms`.
    #[must_use]
    pub const fn period(&self, now_ms: u64) -> u64 {
        pool_period(matches!(self.per, RewardPeriod::Day), now_ms)
    }

    /// The currency budget, as the kernel counts it.
    fn currency_budget(&self) -> u64 {
        u64::try_from(self.currency).unwrap_or(0)
    }

    /// What is left of it at `now_ms`, given what it has spent.
    #[must_use]
    pub fn facts(&self, spent: Option<&PoolSpent>, now_ms: u64) -> PoolFacts {
        let spent = PoolSpent::rolled(spent, self.period(now_ms));
        PoolFacts {
            per: self.per,
            currency: self.currency_budget().saturating_sub(spent.currency),
            items: self
                .items
                .iter()
                .map(|(p, n)| {
                    let used = spent.items.get(p).copied().unwrap_or(0);
                    (p.clone(), u64::from(*n).saturating_sub(used))
                })
                .collect(),
        }
    }

    /// Admits an ask whole against what is left at `now_ms`, and returns
    /// what the pool has spent after it.
    ///
    /// # Errors
    /// [`AskError::Bad`] when the ask could never be paid: it is empty,
    /// negative, too large for one reward, names an item the pool does not
    /// offer, or passes a whole period's budget. [`AskError::Spent`] when
    /// it fits a period's budget but not what is left of this one.
    pub fn admit(
        &self,
        spent: Option<&PoolSpent>,
        now_ms: u64,
        currency: i64,
        items: &BTreeMap<ProtoId, u32>,
    ) -> Result<PoolSpent, AskError> {
        let Ok(want) = u64::try_from(currency) else {
            return Err(AskError::Bad(format!("currency {currency} is negative")));
        };
        if want == 0 && items.values().all(|n| *n == 0) {
            return Err(AskError::Bad("it asks for nothing".to_owned()));
        }
        let count: u64 = items.values().map(|n| u64::from(*n)).sum();
        if count > u64::from(REWARD_ITEMS_MAX) {
            return Err(AskError::Bad(format!(
                "{count} items; one reward makes at most {REWARD_ITEMS_MAX}"
            )));
        }
        let refused = |what: &str, ask: u64, budget: u64, used: u64| {
            if ask > budget {
                AskError::Bad(format!(
                    "asks {ask} {what}; the pool gives at most {budget} {}",
                    self.per_word()
                ))
            } else {
                AskError::Spent(format!(
                    "asks {ask} {what}; {what}: {} left {}",
                    budget.saturating_sub(used),
                    self.left_when()
                ))
            }
        };
        let mut next = PoolSpent::rolled(spent, self.period(now_ms));
        let (fits, after) = pool_admit(next.currency, self.currency_budget(), want);
        if !fits {
            return Err(refused(
                "currency",
                want,
                self.currency_budget(),
                next.currency,
            ));
        }
        next.currency = after;
        for (proto, n) in items.iter().filter(|(_, n)| **n > 0) {
            let Some(budget) = self.items.get(proto).map(|b| u64::from(*b)) else {
                return Err(AskError::Bad(format!("the pool offers no {proto}")));
            };
            let used = next.items.get(proto).copied().unwrap_or(0);
            let (fits, after) = pool_admit(used, budget, u64::from(*n));
            if !fits {
                return Err(refused(proto.as_str(), u64::from(*n), budget, used));
            }
            next.items.insert(proto.clone(), after);
        }
        Ok(next)
    }

    const fn left_when(&self) -> &'static str {
        match self.per {
            RewardPeriod::Day => "today",
            RewardPeriod::Ever => "ever",
        }
    }

    const fn per_word(&self) -> &'static str {
        match self.per {
            RewardPeriod::Day => "a day",
            RewardPeriod::Ever => "ever",
        }
    }
}

/// Why a pool refuses an ask.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AskError {
    /// The ask could never be paid: a script bug, which counts as a fault.
    #[error("{0}")]
    Bad(String),
    /// The ask fits the pool, but not what is left of this period. That is
    /// the world's state, not a script bug: the hook declines, and its
    /// breaker does not move.
    #[error("{0}")]
    Spent(String),
}

/// What a pool has given since the world began. The snapshot keeps these
/// running counts, not the ledger's rows, so it never grows with them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintTotals {
    /// Mints.
    pub mints: u64,
    /// Currency made.
    pub currency: i64,
    /// Items made.
    pub items: u64,
}

impl MintTotals {
    /// These totals with one more mint.
    #[must_use]
    pub fn with(self, m: &MintRecord) -> Self {
        Self {
            mints: self.mints.saturating_add(1),
            currency: self.currency.saturating_add(m.currency),
            items: self
                .items
                .saturating_add(u64::try_from(m.items.len()).unwrap_or(u64::MAX)),
        }
    }
}

/// Whether `id` is a valid pool id: 1 to 32 of `[a-z0-9_]`.
#[must_use]
pub fn valid_pool_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= POOL_ID_MAX
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// What a pool has given in the period it counts in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSpent {
    /// The period: a UTC day of the game clock, or 0 for `ever`.
    pub period: u64,
    /// The currency given in it.
    pub currency: u64,
    /// The items made in it, by prototype.
    #[serde(default)]
    pub items: BTreeMap<ProtoId, u64>,
}

impl PoolSpent {
    /// What a pool has spent, rolled to `period` ([`pool_roll`]).
    #[must_use]
    pub fn rolled(spent: Option<&Self>, period: u64) -> Self {
        let Some(s) = spent else {
            return Self {
                period,
                ..Self::default()
            };
        };
        let (at, currency) = pool_roll(s.period, s.currency, period);
        Self {
            period: at,
            currency,
            items: s
                .items
                .iter()
                .map(|(p, n)| (p.clone(), pool_roll(s.period, *n, period).1))
                .filter(|(_, n)| *n > 0)
                .collect(),
        }
    }
}

/// What is left of a pool, as a hook sees it (`ctx.pools.<pool>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolFacts {
    /// `day` or `ever`.
    pub per: RewardPeriod,
    /// The currency it may still give in this period.
    pub currency: u64,
    /// The items it may still make in this period, by prototype.
    pub items: BTreeMap<ProtoId, u64>,
}

/// One item a reward made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintedItem {
    /// The new instance.
    pub id: ItemId,
    /// Its prototype.
    pub proto: ProtoId,
}

/// One mint, as the ledger keeps it. Every item and every coin a script
/// ever made is in exactly one of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintRecord {
    /// The event that announced it.
    pub seq: Seq,
    /// The pool it came from.
    pub pool: PoolId,
    /// The pool's period it counts in.
    pub period: u64,
    /// The hook that asked for it, such as `verbs.report`.
    pub hook: String,
    /// Who received it.
    pub entity: EntityId,
    /// The currency made.
    pub currency: i64,
    /// The items made.
    pub items: Vec<MintedItem>,
    /// When.
    pub at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn pool(currency: i64, items: &[(&str, u32)], per: RewardPeriod) -> RewardPool {
        RewardPool {
            currency,
            items: items.iter().map(|(p, n)| (ProtoId::from(*p), *n)).collect(),
            per,
        }
    }

    fn ask(items: &[(&str, u32)]) -> BTreeMap<ProtoId, u32> {
        items.iter().map(|(p, n)| (ProtoId::from(*p), *n)).collect()
    }

    #[test]
    fn the_kernels_match_their_spec_table() {
        assert_eq!(pool_period(true, DAY_MS * 3 + 5), 3);
        assert_eq!(pool_period(false, DAY_MS * 3 + 5), 0);
        assert_eq!(pool_roll(3, 40, 4), (4, 0));
        assert_eq!(pool_roll(3, 40, 3), (3, 40));
        assert_eq!(
            pool_roll(3, 40, 2),
            (3, 40),
            "the clock set back refills nothing"
        );
        assert_eq!(pool_admit(0, 60, 20), (true, 20));
        assert_eq!(pool_admit(40, 60, 20), (true, 60));
        assert_eq!(pool_admit(50, 60, 20), (false, 50), "never partly");
        assert_eq!(pool_admit(70, 60, 0), (false, 70));
        assert_eq!(pool_admit(u64::MAX, u64::MAX, u64::MAX), (false, u64::MAX));
        assert_eq!(pool_admit(0, u64::MAX, u64::MAX), (true, u64::MAX));
    }

    #[test]
    fn an_ask_is_admitted_whole_or_not_at_all() {
        let p = pool(60, &[("healing_draught", 3)], RewardPeriod::Day);
        let day = DAY_MS * 19_000;
        let s = p
            .admit(None, day, 20, &ask(&[("healing_draught", 1)]))
            .unwrap();
        assert_eq!(s.currency, 20);
        assert_eq!(s.items[&ProtoId::from("healing_draught")], 1);
        assert_eq!(s.period, 19_000);
        let s = p
            .admit(Some(&s), day + 5, 40, &ask(&[("healing_draught", 2)]))
            .unwrap();
        assert_eq!(
            p.admit(Some(&s), day + 9, 1, &BTreeMap::new()).unwrap_err(),
            AskError::Spent("asks 1 currency; currency: 0 left today".into())
        );
        assert_eq!(
            p.admit(Some(&s), day + 9, 0, &ask(&[("healing_draught", 1)]))
                .unwrap_err(),
            AskError::Spent("asks 1 healing_draught; healing_draught: 0 left today".into())
        );
        // An ask past a whole day's budget can never be paid: a bug.
        assert_eq!(
            p.admit(None, day, 61, &BTreeMap::new()).unwrap_err(),
            AskError::Bad("asks 61 currency; the pool gives at most 60 a day".into())
        );
        assert_eq!(
            p.admit(None, day, 0, &ask(&[("healing_draught", 4)]))
                .unwrap_err()
                .to_string(),
            "asks 4 healing_draught; the pool gives at most 3 a day"
        );
        let facts = p.facts(Some(&s), day + 9);
        assert_eq!(
            (
                facts.currency,
                facts.items[&ProtoId::from("healing_draught")]
            ),
            (0, 0)
        );
        // The next day the pool refills.
        let facts = p.facts(Some(&s), day + DAY_MS);
        assert_eq!(
            (
                facts.currency,
                facts.items[&ProtoId::from("healing_draught")]
            ),
            (60, 3)
        );
        assert!(
            p.admit(Some(&s), day + DAY_MS, 60, &BTreeMap::new())
                .is_ok()
        );
        // An ever pool never refills.
        let e = pool(10, &[], RewardPeriod::Ever);
        let s = e.admit(None, 0, 10, &BTreeMap::new()).unwrap();
        assert_eq!(
            e.admit(Some(&s), DAY_MS * 400, 1, &BTreeMap::new())
                .unwrap_err(),
            AskError::Spent("asks 1 currency; currency: 0 left ever".into())
        );
        assert_eq!(
            e.admit(None, 0, 11, &BTreeMap::new()).unwrap_err(),
            AskError::Bad("asks 11 currency; the pool gives at most 10 ever".into())
        );
        let bad = |r: Result<PoolSpent, AskError>| match r {
            Err(AskError::Bad(m)) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            bad(e.admit(None, 0, -1, &BTreeMap::new())),
            "currency -1 is negative"
        );
        assert_eq!(
            bad(e.admit(None, 0, 0, &ask(&[("x", 0)]))),
            "it asks for nothing"
        );
        assert_eq!(
            bad(e.admit(None, 0, 1, &ask(&[("x", 1)]))),
            "the pool offers no x"
        );
        let big = pool(0, &[("ale", 100)], RewardPeriod::Day);
        assert_eq!(
            bad(big.admit(None, 0, 0, &ask(&[("ale", 17)]))),
            "17 items; one reward makes at most 16"
        );
        assert!(valid_pool_id("warden_bounty"));
        assert!(!valid_pool_id("Bad"));
        assert!(!valid_pool_id(""));
        assert!(!valid_pool_id(&"a".repeat(33)));
        assert_eq!(RewardPeriod::Day.name(), "day");
        assert_eq!(RewardPeriod::Ever.name(), "ever");
    }

    proptest! {
        #[test]
        fn no_run_of_asks_passes_a_budget_and_each_mint_counts_exactly(
            budget in 0i64..200,
            steps in prop::collection::vec((0u64..5, 0i64..40, 0u32..4), 0..80),
        ) {
            let p = pool(budget, &[("ale", 5)], RewardPeriod::Day);
            let mut spent: Option<PoolSpent> = None;
            let mut day = 0u64;
            // What the ledger would hold, by period.
            let mut ledger: BTreeMap<u64, (i64, u64)> = BTreeMap::new();
            for (jump, money, ales) in steps {
                // The clock moves forward, stays, or is set back.
                day = match jump { 0 => day.saturating_sub(1), 1 | 2 => day, _ => day + 1 };
                let want = ask(&[("ale", ales)]);
                if let Ok(next) = p.admit(spent.as_ref(), day * DAY_MS, money, &want) {
                    let e = ledger.entry(next.period).or_default();
                    e.0 += money;
                    e.1 += u64::from(ales);
                    spent = Some(next);
                }
                if let Some(s) = &spent {
                    prop_assert!(s.currency <= u64::try_from(budget).unwrap());
                    prop_assert!(s.items.get(&ProtoId::from("ale")).copied().unwrap_or(0) <= 5);
                    let (m, a) = ledger.get(&s.period).copied().unwrap_or_default();
                    prop_assert_eq!(u64::try_from(m).unwrap(), s.currency, "the ledger counts every coin");
                    prop_assert_eq!(a, s.items.get(&ProtoId::from("ale")).copied().unwrap_or(0));
                }
            }
            for (m, a) in ledger.values() {
                prop_assert!(*m <= budget && *a <= 5, "no period passes its budget");
            }
        }
    }
}
