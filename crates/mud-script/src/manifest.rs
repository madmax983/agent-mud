//! `scripts.toml`: which script handles which hook.
//!
//! The manifest belongs to the script layer, not to the world pack, so it
//! reloads together with the `.rhai` files.

use std::collections::BTreeMap;

use mud_world::{EntityId, ProtoId, RoomId};
use serde::{Deserialize, Serialize};

use crate::limits::LimitsDef;

/// The whole manifest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Manifest {
    /// `[scripts]`.
    pub scripts: ScriptsTable,
    /// `[limits]`.
    pub limits: LimitsDef,
    /// `[minds.<cast id>]`.
    pub minds: BTreeMap<EntityId, MindBinding>,
    /// `[rooms.<room id>]`.
    pub rooms: BTreeMap<RoomId, RoomBinding>,
    /// `[items.<proto id>]`.
    pub items: BTreeMap<ProtoId, ItemBinding>,
    /// `[[verbs]]`.
    pub verbs: Vec<VerbBinding>,
}

/// `[scripts]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScriptsTable {
    /// A library merged into every script; the file's own functions win.
    pub lib: Option<String>,
}

/// The runtime a mind binding asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MindRuntime {
    /// Run this entity on its Rhai script, whatever `[minds]` maps its
    /// tier to.
    Rhai,
}

/// `[minds.<cast id>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MindBinding {
    /// The script; it must define `fn on_wake(ctx)`.
    pub file: String,
    /// `"rhai"` to run the entity on this script whatever its tier's
    /// runtime is.
    #[serde(default)]
    pub runtime: Option<MindRuntime>,
    /// The memory schema; a change starts the mind's memory fresh.
    #[serde(default = "one")]
    pub memory_schema: u32,
    /// Constants for the script (`ctx.config`).
    #[serde(default)]
    pub config: toml::Table,
}

const fn one() -> u32 {
    1
}

/// `[rooms.<room id>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomBinding {
    /// The script.
    pub file: String,
    /// Runs `fn on_enter(ctx)` when someone walks in.
    #[serde(default)]
    pub on_enter: bool,
    /// Runs `fn on_speech(ctx)` when someone speaks.
    #[serde(default)]
    pub on_speech: bool,
    /// Constants for the script.
    #[serde(default)]
    pub config: toml::Table,
}

/// `[items.<proto id>]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemBinding {
    /// The script.
    pub file: String,
    /// Runs `fn on_take(ctx)` after someone picks one up.
    #[serde(default)]
    pub on_take: bool,
    /// Runs `fn on_use(ctx)` when someone uses one (items with no engine
    /// use only).
    #[serde(default)]
    pub on_use: bool,
    /// Constants for the script.
    #[serde(default)]
    pub config: toml::Table,
}

/// `[[verbs]]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerbBinding {
    /// The verb: 2 to 16 lowercase letters or underscores.
    pub name: String,
    /// How to type it, for `help` (at most 60 printable characters).
    pub usage: String,
    /// Where it works; empty means anywhere.
    #[serde(default)]
    pub rooms: Vec<RoomId>,
    /// The script.
    pub file: String,
    /// The function; `on_<name>` by default.
    #[serde(default, rename = "fn")]
    pub func: Option<String>,
    /// Constants for the script.
    #[serde(default)]
    pub config: toml::Table,
}

impl VerbBinding {
    /// The function the verb runs.
    #[must_use]
    pub fn function(&self) -> String {
        self.func
            .clone()
            .unwrap_or_else(|| format!("on_{}", self.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_reads_every_table_and_refuses_unknown_keys() {
        let m: Manifest = toml::from_str(
            r#"
[scripts]
lib = "lib.rhai"
[limits]
max_effects = 4
[minds.guard_01]
file = "minds/hale.rhai"
runtime = "rhai"
config = { night_from = 20 }
[rooms.ossuary_crypt]
file = "rooms/ossuary.rhai"
on_enter = true
[items.iron_ore]
file = "items/ore.rhai"
on_take = true
[[verbs]]
name = "pray"
usage = "pray"
file = "rooms/ossuary.rhai"
"#,
        )
        .unwrap();
        let hale = &m.minds[&EntityId::from("guard_01")];
        assert_eq!(hale.runtime, Some(MindRuntime::Rhai));
        assert_eq!(hale.memory_schema, 1);
        assert_eq!(m.verbs[0].function(), "on_pray");
        assert_eq!(m.limits.max_effects, Some(4));
        let bad = toml::from_str::<Manifest>("[rooms.x]\nfile = \"a.rhai\"\non_exit = true\n");
        assert!(bad.unwrap_err().to_string().contains("on_exit"));
        let bad = toml::from_str::<Manifest>("[minds.x]\nfile = \"a.rhai\"\nruntime = \"lua\"\n");
        assert!(bad.is_err());
    }
}
