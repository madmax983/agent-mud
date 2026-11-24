//! [`ScriptHost`]: the compiled scripts of a world, swappable at runtime.
//!
//! Lock order: the world lock, then the host lock. A hook runs inside
//! `World::execute` and only clones the compiled scripts under the read
//! lock. A mind wake clones them, drops the lock, and only then reads the
//! world. A reload compiles with no lock held and installs under the
//! world's write lock.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use mud_world::hooks::{
    HookId, HookInput, HookLimits, HookOutcome, HookReply, ScriptEffect, VerbInfo, WorldDice,
    WorldHooks, hook_step,
};
use mud_world::{EntityId, WorldPack};
use rhai::{AST, Array, Dynamic, Engine, Map};
use serde_json::Value;

use crate::api::Effect;
use crate::error::ScriptError;
use crate::limits::Limits;
use crate::load::{ScriptSources, compile};
use crate::manifest::MindRuntime;
use crate::sandbox::{self, CallKind, CallState};
use crate::untrusted::Untrusted;
use crate::view::View;

/// One world hook's script.
#[derive(Debug, Clone)]
pub struct HookEntry {
    /// The file, merged with the library.
    pub ast: Arc<AST>,
    /// The function.
    pub func: String,
    /// `ctx.config`.
    pub config: Arc<Value>,
}

/// One mind's script.
#[derive(Debug, Clone)]
pub struct MindEntry {
    /// The file, merged with the library.
    pub ast: Arc<AST>,
    /// `ctx.config`.
    pub config: Arc<Value>,
    /// `runtime = "rhai"`, if set.
    pub runtime: Option<MindRuntime>,
    /// The memory schema.
    pub memory_schema: u32,
}

/// A checked, compiled script version.
pub struct Compiled {
    /// The content hash.
    pub version: String,
    /// How many files it has.
    pub files: usize,
    /// The pack's limits.
    pub limits: Limits,
    /// The sandboxed engine.
    pub engine: Engine,
    /// The world hooks.
    pub hooks: BTreeMap<HookId, HookEntry>,
    /// The mind scripts.
    pub minds: BTreeMap<EntityId, MindEntry>,
    /// The custom verbs.
    pub verbs: BTreeMap<String, VerbInfo>,
    /// Warnings from loading.
    pub warnings: Vec<String>,
}

impl fmt::Debug for Compiled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compiled")
            .field("version", &self.version)
            .field("files", &self.files)
            .field(
                "hooks",
                &self.hooks.keys().map(HookId::key).collect::<Vec<_>>(),
            )
            .field("minds", &self.minds.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn type_name(d: &Dynamic) -> String {
    d.type_name()
        .rsplit("::")
        .next()
        .unwrap_or("value")
        .to_owned()
}

fn view_of<T: serde::Serialize>(v: &T) -> Result<Dynamic, String> {
    serde_json::to_value(v)
        .map(|v| Dynamic::from(View::new(v)))
        .map_err(|e| format!("building the view: {e}"))
}

fn effects_of(hook: &str, v: Dynamic) -> Result<Vec<ScriptEffect>, String> {
    if v.is_unit() {
        return Ok(Vec::new());
    }
    if !v.is_array() {
        return Err(format!(
            "{hook} returned a {}, not an array of effects",
            type_name(&v)
        ));
    }
    let list: Array = v.cast();
    list.into_iter()
        .enumerate()
        .map(|(i, d)| {
            if d.is::<Effect>() {
                Ok(d.cast::<Effect>().0)
            } else {
                Err(format!("effect {i} is a {}, not an effect", type_name(&d)))
            }
        })
        .collect()
}

impl Compiled {
    /// A version with no scripts (the pack has no `scripts.toml`).
    #[must_use]
    pub fn empty(version: String, warnings: Vec<String>) -> Self {
        let limits = Limits::default();
        Self {
            version,
            files: 0,
            limits,
            engine: sandbox::engine(&limits),
            hooks: BTreeMap::new(),
            minds: BTreeMap::new(),
            verbs: BTreeMap::new(),
            warnings,
        }
    }

    /// The entities whose binding asks for `runtime = "rhai"`.
    #[must_use]
    pub fn rhai_runtime_minds(&self) -> BTreeSet<EntityId> {
        self.minds
            .iter()
            .filter(|(_, m)| m.runtime == Some(MindRuntime::Rhai))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// A one-line summary, such as `rhai-… (6 files: 1 mind, 2 rooms, 2
    /// items, 2 verbs)`.
    #[must_use]
    pub fn summary(&self) -> String {
        let rooms: BTreeSet<String> = self
            .hooks
            .keys()
            .filter_map(|h| match h {
                HookId::Enter(r) | HookId::Speech(r) => Some(r.to_string()),
                _ => None,
            })
            .collect();
        let items: BTreeSet<String> = self
            .hooks
            .keys()
            .filter_map(|h| match h {
                HookId::Take(p) | HookId::Use(p) => Some(p.to_string()),
                _ => None,
            })
            .collect();
        let plural = |n: usize, one: &str| {
            if n == 1 {
                format!("{n} {one}")
            } else {
                format!("{n} {one}s")
            }
        };
        format!(
            "{} ({}: {}, {}, {}, {})",
            self.version,
            plural(self.files, "file"),
            plural(self.minds.len(), "mind"),
            plural(rooms.len(), "room"),
            plural(items.len(), "item"),
            plural(self.verbs.len(), "verb")
        )
    }

    /// Runs a world hook. Pure: it reads `input` and returns effects and
    /// the dice advanced by exactly the rolls made.
    #[must_use]
    pub fn call_hook(&self, hook: &HookId, input: &HookInput, dice: WorldDice) -> HookReply {
        let key = hook.key();
        let Some(entry) = self.hooks.get(hook) else {
            return HookReply {
                outcome: Err(format!("no script handles {key}")),
                log: Vec::new(),
            };
        };
        let ctx = match hook_ctx(input, entry) {
            Ok(c) => c,
            Err(e) => {
                return HookReply {
                    outcome: Err(e),
                    log: Vec::new(),
                };
            }
        };
        let state = CallState::new(CallKind::Hook, dice, &self.limits);
        let out = sandbox::call(&self.engine, &entry.ast, &entry.func, ctx, state);
        let log = out.state.finished_log();
        let dice = out.state.dice;
        HookReply {
            outcome: out
                .result
                .and_then(|v| effects_of(&key, v))
                .map(|fx| (fx, dice)),
            log,
        }
    }
}

fn hook_ctx(input: &HookInput, entry: &HookEntry) -> Result<Dynamic, String> {
    let mut m = Map::new();
    m.insert("hook".into(), Dynamic::from(input.hook.clone()));
    m.insert(
        "now_ms".into(),
        Dynamic::from_int(i64::try_from(input.now_ms).unwrap_or(i64::MAX)),
    );
    m.insert("actor".into(), view_of(&input.actor)?);
    m.insert("room".into(), view_of(&input.room)?);
    m.insert(
        "from".into(),
        input
            .from
            .as_ref()
            .map_or(Dynamic::UNIT, |r| Dynamic::from(r.to_string())),
    );
    m.insert(
        "text".into(),
        Dynamic::from(Untrusted::new(0, input.text.as_deref().unwrap_or(""))),
    );
    m.insert(
        "target".into(),
        input.target.as_ref().map_or(Ok(Dynamic::UNIT), view_of)?,
    );
    m.insert(
        "item".into(),
        input.item.as_ref().map_or(Ok(Dynamic::UNIT), view_of)?,
    );
    m.insert("flags".into(), view_of(&input.flags)?);
    m.insert("cast".into(), view_of(&input.cast)?);
    m.insert(
        "config".into(),
        Dynamic::from(View::new((*entry.config).clone())),
    );
    Ok(Dynamic::from_map(m))
}

/// Where the scripts come from.
#[derive(Debug, Clone)]
enum Source {
    /// A pack folder, which a reload reads again.
    Dir(PathBuf),
    /// Compiled into the binary.
    Bundled,
}

/// The scripts of a running world.
///
/// It also keeps the mind breakers (consecutive faults per mind script),
/// in memory: a restart, a reload and a re-arm reopen them. Lock order:
/// the mind breakers, then the compiled scripts.
#[derive(Debug)]
pub struct ScriptHost {
    source: Source,
    compiled: RwLock<Arc<Compiled>>,
    reload: tokio::sync::Mutex<()>,
    reloads: AtomicU64,
    mind_faults: Mutex<BTreeMap<EntityId, u8>>,
}

impl ScriptHost {
    fn new(source: Source, compiled: Compiled) -> Self {
        Self {
            source,
            compiled: RwLock::new(Arc::new(compiled)),
            reload: tokio::sync::Mutex::new(()),
            reloads: AtomicU64::new(0),
            mind_faults: Mutex::new(BTreeMap::new()),
        }
    }

    fn mind_faults_mut(&self) -> std::sync::MutexGuard<'_, BTreeMap<EntityId, u8>> {
        self.mind_faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Loads and checks the scripts of the pack in `dir`.
    ///
    /// # Errors
    /// Every problem found.
    pub fn load(dir: &Path, pack: &WorldPack) -> Result<Self, ScriptError> {
        let sources = ScriptSources::read_dir(dir)?;
        Ok(Self::new(
            Source::Dir(dir.to_path_buf()),
            compile(&sources, pack)?,
        ))
    }

    /// The scripts of the bundled crypt-v01 pack.
    ///
    /// # Errors
    /// Only if the bundled scripts are broken, which a test forbids.
    pub fn bundled_crypt_v01(pack: &WorldPack) -> Result<Self, ScriptError> {
        Self::from_sources(&crate::crypt::crypt_v01_sources(), pack)
    }

    /// Scripts from text (they cannot be reloaded).
    ///
    /// # Errors
    /// Every problem found.
    pub fn from_sources(sources: &ScriptSources, pack: &WorldPack) -> Result<Self, ScriptError> {
        Ok(Self::new(Source::Bundled, compile(sources, pack)?))
    }

    /// Reads, compiles, validates and dry-runs the pack folder again, for a
    /// reload. Pure CPU and file reads: run it off the async workers.
    ///
    /// # Errors
    /// The scripts are bundled, or invalid, or they change which entities
    /// run on Rhai (that needs a restart).
    pub fn compile_from_source(&self, pack: &Arc<WorldPack>) -> Result<Compiled, ScriptError> {
        let Source::Dir(dir) = &self.source else {
            return Err(ScriptError::Bundled);
        };
        let compiled = compile(&ScriptSources::read_dir(dir)?, pack)?;
        let mut problems = compiled.dry_run(pack);
        let before = self.current().rhai_runtime_minds();
        let after = compiled.rhai_runtime_minds();
        for id in before.symmetric_difference(&after) {
            problems.push(format!("minds.{id}: runtime changes need a restart"));
        }
        if problems.is_empty() {
            Ok(compiled)
        } else {
            Err(ScriptError::Invalid(problems))
        }
    }

    /// Swaps in a new version. Wakes already running finish on the old.
    /// Every mind breaker reopens.
    pub fn install(&self, compiled: Compiled) {
        let mut faults = self.mind_faults_mut();
        *self
            .compiled
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(compiled);
        self.reloads.fetch_add(1, Ordering::SeqCst);
        faults.clear();
    }

    /// Re-arms the scripts running now, as a reload with nothing new to
    /// read (the bundled pack): every mind breaker reopens, and the reload
    /// count moves on.
    pub fn rearm(&self) {
        let mut faults = self.mind_faults_mut();
        self.reloads.fetch_add(1, Ordering::SeqCst);
        faults.clear();
    }

    /// `e`'s consecutive mind script faults.
    #[must_use]
    pub fn mind_faults(&self, e: &EntityId) -> u8 {
        self.mind_faults_mut().get(e).copied().unwrap_or(0)
    }

    /// Every mind with faults, and how many.
    #[must_use]
    pub fn mind_breakers(&self) -> BTreeMap<EntityId, u8> {
        self.mind_faults_mut().clone()
    }

    /// Steps `e`'s mind breaker by the outcome of a wake that started at
    /// reload count `since`. A wake that started before a reload or a
    /// re-arm counts for nothing: it returns `None`. Otherwise it returns
    /// the faults now and whether the mind may still run its script.
    pub fn step_mind(&self, e: &EntityId, since: u64, outcome: HookOutcome) -> Option<(u8, bool)> {
        let mut faults = self.mind_faults_mut();
        if self.reloads() != since {
            return None;
        }
        let max = self.current().limits.max_consecutive_faults;
        let (next, allowed) = hook_step(faults.get(e).copied().unwrap_or(0), outcome, max);
        if next == 0 {
            faults.remove(e);
        } else {
            faults.insert(e.clone(), next);
        }
        drop(faults);
        Some((next, allowed))
    }

    /// The mutex a reload holds, so two reloads never interleave.
    #[must_use]
    pub const fn reload_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.reload
    }

    /// How many versions were installed since start.
    #[must_use]
    pub fn reloads(&self) -> u64 {
        self.reloads.load(Ordering::SeqCst)
    }

    /// Whether the scripts can be reloaded from a folder.
    #[must_use]
    pub const fn reloadable(&self) -> bool {
        matches!(self.source, Source::Dir(_))
    }

    /// The version running now.
    #[must_use]
    pub fn current(&self) -> Arc<Compiled> {
        Arc::clone(&self.compiled.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The version hash.
    #[must_use]
    pub fn version(&self) -> String {
        self.current().version.clone()
    }

    /// The one-line summary.
    #[must_use]
    pub fn summary(&self) -> String {
        self.current().summary()
    }

    /// Runs every hook and mind once on probe inputs.
    #[must_use]
    pub fn dry_run(&self, pack: &Arc<WorldPack>) -> Vec<String> {
        self.current().dry_run(pack)
    }

    /// Whether `e` has a mind script.
    #[must_use]
    pub fn has_mind(&self, e: &EntityId) -> bool {
        self.current().minds.contains_key(e)
    }

    /// The runtime `e`'s binding asks for.
    #[must_use]
    pub fn mind_runtime(&self, e: &EntityId) -> Option<MindRuntime> {
        self.current().minds.get(e).and_then(|m| m.runtime)
    }

    /// `e`'s memory schema (1 when it has no script).
    #[must_use]
    pub fn mind_memory_schema(&self, e: &EntityId) -> u32 {
        self.current().minds.get(e).map_or(1, |m| m.memory_schema)
    }

    /// The entities that run on Rhai whatever their tier.
    #[must_use]
    pub fn rhai_runtime_minds(&self) -> BTreeSet<EntityId> {
        self.current().rhai_runtime_minds()
    }

    /// Every hook key, for the observer.
    #[must_use]
    pub fn hook_keys(&self) -> Vec<String> {
        self.current().hooks.keys().map(HookId::key).collect()
    }

    /// Every mind with a script.
    #[must_use]
    pub fn minds(&self) -> Vec<EntityId> {
        self.current().minds.keys().cloned().collect()
    }

    /// Runs `e`'s mind script for one wake.
    #[must_use]
    pub fn on_wake(&self, wake: &crate::mind::MindWake) -> crate::mind::MindReply {
        self.current().on_wake(wake)
    }
}

impl WorldHooks for ScriptHost {
    fn version(&self) -> String {
        Self::version(self)
    }

    fn has(&self, hook: &HookId) -> bool {
        self.current().hooks.contains_key(hook)
    }

    fn verb(&self, name: &str) -> Option<VerbInfo> {
        self.current().verbs.get(name).cloned()
    }

    fn verbs(&self) -> Vec<VerbInfo> {
        self.current().verbs.values().cloned().collect()
    }

    fn limits(&self) -> HookLimits {
        self.current().limits.hook_limits()
    }

    fn call(&self, hook: &HookId, input: &HookInput, dice: WorldDice) -> HookReply {
        self.current().call_hook(hook, input, dice)
    }
}
