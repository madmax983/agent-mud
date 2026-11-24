//! The sandbox: one Rhai engine with fixed limits and curated natives, and
//! the runner every script call goes through.
//!
//! - The engine starts from `Engine::new_raw`, never `Engine::new` or the
//!   standard package: `LanguageCorePackage` alone carries `sleep`, and the
//!   std collection packages hold natives that cost far more than one
//!   operation each. Only arithmetic (checked), logic and range iteration
//!   are loaded; [`crate::api`] adds a small, measured set of natives.
//! - Every call is bounded by the operation limit and by a deterministic
//!   work counter that the natives whose cost grows with their input
//!   charge. There is no wall-clock deadline.
//! - Rhai checks the size of a value it builds, not of a map or array
//!   grown in place by index assignment (`m[k] = v`), and copying a value
//!   (`let b = m`, a function argument) costs one operation whatever its
//!   size. So every read of a variable whose value is over the caps
//!   charges the work counter by its size ([`charge_read`]). `this` is
//!   disabled: a method call binds it without a variable read.
//! - `eval`, `print` and `debug` are disabled; floats, time, modules and
//!   custom syntax are compiled out (see the workspace `Cargo.toml`).
//! - Every call runs under `catch_unwind`: a panic is a fault.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};

use mud_world::hooks::WorldDice;
use rhai::packages::{ArithmeticPackage, BasicIteratorPackage, LogicPackage, Package};
use rhai::{
    AST, CallFnOptions, Dynamic, Engine, EvalAltResult, EvalContext, NativeCallContext, Position,
    Scope,
};

use crate::limits::Limits;

/// The result of a native function.
pub type RhaiResult<T> = Result<T, Box<EvalAltResult>>;

/// The deepest script call stack.
pub const MAX_CALL_LEVELS: usize = 16;
/// The deepest expression, at global and at function level.
pub const MAX_EXPR_DEPTHS: (usize, usize) = (64, 32);
/// The longest string, in bytes.
pub const MAX_STRING_SIZE: usize = 1_024;
/// The longest array, nested arrays counted in aggregate.
pub const MAX_ARRAY_SIZE: usize = 64;
/// The largest map, nested maps counted in aggregate.
pub const MAX_MAP_SIZE: usize = 64;
/// The most variables in scope.
pub const MAX_VARIABLES: usize = 128;
/// The most script functions.
pub const MAX_FUNCTIONS: usize = 256;
/// The most log lines one call keeps.
pub const LOG_LINES_MAX: usize = 16;
/// The longest log line kept.
pub const LOG_LINE_MAX: usize = 200;

/// Which kind of script a call runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// A world hook: it returns effects.
    Hook,
    /// A mind script: it returns command lines.
    Mind,
}

/// What one call carries besides its arguments.
#[derive(Debug, Clone)]
pub struct CallState {
    /// The kind of call.
    pub kind: CallKind,
    /// The dice it rolls: a clone of the world's for a hook, seeded ones
    /// for a mind.
    pub dice: WorldDice,
    /// Its log lines.
    pub log: Vec<String>,
    /// Log lines dropped past [`LOG_LINES_MAX`].
    pub log_dropped: usize,
    /// Work units spent.
    pub work: u64,
    /// Work units allowed.
    pub work_limit: u64,
    /// Search words tested per `Untrusted` value.
    pub probes: BTreeMap<u32, u32>,
}

impl CallState {
    /// A fresh state.
    #[must_use]
    pub const fn new(kind: CallKind, dice: WorldDice, limits: &Limits) -> Self {
        Self {
            kind,
            dice,
            log: Vec::new(),
            log_dropped: 0,
            work: 0,
            work_limit: limits.max_operations,
            probes: BTreeMap::new(),
        }
    }

    /// The log, with a last line counting what was dropped.
    #[must_use]
    pub fn finished_log(&self) -> Vec<String> {
        let mut out = self.log.clone();
        if self.log_dropped > 0 {
            out.push(format!("…({} more)", self.log_dropped));
        }
        out
    }
}

/// The call state, shared with the natives through the call's tag.
#[derive(Debug, Clone)]
pub struct StateHandle(pub Arc<Mutex<CallState>>);

/// A runtime fault with `message`.
pub fn fault(message: impl Into<String>) -> Box<EvalAltResult> {
    EvalAltResult::ErrorRuntime(Dynamic::from(message.into()), Position::NONE).into()
}

fn with_tag<R>(
    tag: Option<&Dynamic>,
    f: impl FnOnce(&mut CallState) -> Result<R, String>,
) -> RhaiResult<R> {
    let handle = tag
        .and_then(|t| t.clone().try_cast::<StateHandle>())
        .ok_or_else(|| fault("this function needs a script call"))?;
    let mut state = handle.0.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut state).map_err(fault)
}

/// Runs `f` on the call state of `ctx`.
///
/// # Errors
/// `f` fails, or the call has no state.
pub fn with_state<R>(
    ctx: &NativeCallContext,
    f: impl FnOnce(&mut CallState) -> Result<R, String>,
) -> RhaiResult<R> {
    with_tag(ctx.tag(), f)
}

fn charge_state(s: &mut CallState, n: usize) -> Result<(), String> {
    s.work = s.work.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    if s.work > s.work_limit {
        Err(format!("the script did too much work ({} units)", s.work))
    } else {
        Ok(())
    }
}

/// Charges `n` work units.
///
/// # Errors
/// The call has done more work than its limit.
pub fn charge(ctx: &NativeCallContext, n: usize) -> RhaiResult<()> {
    with_state(ctx, |s| charge_state(s, n))
}

/// The bytes of string one work unit pays for.
pub const STRING_BYTES_PER_UNIT: usize = 64;

/// The aggregate sizes of a value, as Rhai counts them for its caps.
///
/// Array elements, map entries and string bytes, nested ones included.
/// The walk stops once it has visited more than `stop` containers' entries,
/// so it never costs much more than the caller can charge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sizes {
    /// Array elements.
    pub arrays: usize,
    /// Map entries.
    pub maps: usize,
    /// String bytes.
    pub strings: usize,
}

impl Sizes {
    /// The sizes of `v`, walking at most about `stop` entries.
    #[must_use]
    pub fn of(v: &Dynamic, stop: usize) -> Self {
        let mut s = Self::default();
        if v.is_shared() {
            if let Some(inner) = v.read_lock::<Dynamic>() {
                s.add(&inner, stop);
            }
        } else {
            s.add(v, stop);
        }
        s
    }

    const fn entries(&self) -> usize {
        self.arrays.saturating_add(self.maps)
    }

    fn add(&mut self, v: &Dynamic, stop: usize) {
        if self.entries() > stop {
            return;
        }
        if v.is_map() {
            if let Ok(m) = v.as_map_ref() {
                self.maps = self.maps.saturating_add(m.len());
                for x in m.values().filter(|x| Self::sized(x)) {
                    self.add(x, stop);
                }
            }
        } else if v.is_array() {
            if let Ok(a) = v.as_array_ref() {
                self.arrays = self.arrays.saturating_add(a.len());
                for x in a.iter().filter(|x| Self::sized(x)) {
                    self.add(x, stop);
                }
            }
        } else if let Ok(t) = v.as_immutable_string_ref() {
            self.strings = self.strings.saturating_add(t.len());
        }
    }

    /// Whether `v` has a size: a map, an array or a string.
    fn sized(v: &Dynamic) -> bool {
        v.is_map() || v.is_array() || v.is_string()
    }

    /// Whether every size is within the sandbox caps.
    #[must_use]
    pub const fn within_caps(&self) -> bool {
        self.arrays <= MAX_ARRAY_SIZE
            && self.maps <= MAX_MAP_SIZE
            && self.strings <= MAX_STRING_SIZE
    }

    /// The work units a read of a value this size costs: nothing within
    /// the caps, else one per entry and one per
    /// [`STRING_BYTES_PER_UNIT`] bytes of string.
    #[must_use]
    pub const fn read_cost(&self) -> usize {
        if self.within_caps() {
            0
        } else {
            self.entries()
                .saturating_add(self.strings.div_ceil(STRING_BYTES_PER_UNIT))
        }
    }
}

/// Charges a read of the variable `name` by the size of its value (see
/// [`Sizes::read_cost`]). Never resolves the variable itself.
///
/// # Errors
/// The read takes the call over its work limit.
fn charge_read(name: &str, ctx: &EvalContext) -> RhaiResult<Option<Dynamic>> {
    let Some(v) = ctx
        .scope()
        .get(name)
        .filter(|v| Sizes::sized(v) || v.is_shared())
    else {
        return Ok(None);
    };
    // Within the caps a read is free, and the walk is short: the caps
    // bound it. Over them, the walk stops a little past what is left.
    let within = Sizes::of(v, MAX_ARRAY_SIZE + MAX_MAP_SIZE);
    if within.within_caps() {
        return Ok(None);
    }
    with_tag(Some(ctx.tag()), |s| {
        let left = usize::try_from(s.work_limit.saturating_sub(s.work)).unwrap_or(usize::MAX);
        charge_state(s, Sizes::of(v, left).read_cost())
    })?;
    Ok(None)
}

/// The sandboxed engine.
#[must_use]
pub fn engine(limits: &Limits) -> Engine {
    let mut e = Engine::new_raw();
    ArithmeticPackage::new().register_into_engine(&mut e);
    LogicPackage::new().register_into_engine(&mut e);
    BasicIteratorPackage::new().register_into_engine(&mut e);
    for symbol in ["eval", "print", "debug", "this"] {
        e.disable_symbol(symbol);
    }
    e.set_strict_variables(true);
    e.set_fail_on_invalid_map_property(true);
    e.set_allow_looping(true);
    e.set_allow_anonymous_fn(false);
    // Registered operators (the charged `+=` on arrays) win over the
    // built-in fast paths.
    e.set_fast_operators(false);
    // `unchecked` compiles these out; the build then fails here.
    e.set_max_call_levels(MAX_CALL_LEVELS);
    e.set_max_expr_depths(MAX_EXPR_DEPTHS.0, MAX_EXPR_DEPTHS.1);
    e.set_max_string_size(MAX_STRING_SIZE);
    e.set_max_array_size(MAX_ARRAY_SIZE);
    e.set_max_map_size(MAX_MAP_SIZE);
    e.set_max_variables(MAX_VARIABLES);
    e.set_max_functions(MAX_FUNCTIONS);
    e.set_max_operations(limits.max_operations);
    #[expect(
        deprecated,
        reason = "rhai marks on_var volatile, not deprecated; rhai is pinned to =1.27.0"
    )]
    e.on_var(|name, _, ctx| charge_read(name, &ctx));
    crate::api::register(&mut e);
    e
}

/// An engine that stops at the first operation, to find top-level code.
fn probe_engine() -> Engine {
    let mut e = Engine::new_raw();
    e.on_progress(|_| Some(Dynamic::UNIT));
    e
}

/// Refuses a file that runs code at the top level: a script is functions
/// only, so loading it can do nothing.
///
/// # Errors
/// The file has a top-level statement, `let` or `const`.
pub fn refuse_top_level(ast: &AST) -> Result<(), String> {
    let mut bare = ast.clone();
    bare.clear_functions();
    match probe_engine().run_ast(&bare) {
        Ok(()) => Ok(()),
        Err(_) => Err("top-level code is not allowed; put it in a function".to_owned()),
    }
}

/// A readable message for a script error, without Rhai's call wrappers.
#[must_use]
pub fn describe(err: &EvalAltResult) -> String {
    match err {
        EvalAltResult::ErrorInFunctionCall(_, _, inner, _) => describe(inner),
        EvalAltResult::ErrorRuntime(v, pos) => {
            let msg = v.clone().into_string().unwrap_or_else(|_| v.to_string());
            if pos.is_none() {
                msg
            } else {
                format!("{msg} ({pos})")
            }
        }
        other => other.to_string(),
    }
}

/// What a call came to.
#[derive(Debug)]
pub struct CallOutput {
    /// The returned value, or why the call faulted.
    pub result: Result<Dynamic, String>,
    /// The state afterwards: the dice advanced by exactly the rolls made,
    /// the log.
    pub state: CallState,
}

/// Calls `name(arg)` in `ast`. Never panics; a panic inside is a fault.
#[must_use]
pub fn call(engine: &Engine, ast: &AST, name: &str, arg: Dynamic, state: CallState) -> CallOutput {
    let handle = StateHandle(Arc::new(Mutex::new(state)));
    let options = CallFnOptions::new()
        .eval_ast(false)
        .rewind_scope(true)
        .with_tag(handle.clone());
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        engine.call_fn_with_options::<Dynamic>(options, &mut Scope::new(), ast, name, (arg,))
    }))
    .map_err(|_| "the script engine panicked".to_owned())
    .and_then(|r| r.map_err(|e| describe(&e)));
    let state = match Arc::try_unwrap(handle.0) {
        Ok(m) => m.into_inner().unwrap_or_else(PoisonError::into_inner),
        Err(shared) => shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
    };
    CallOutput { result, state }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhai::{Array, Map};

    #[test]
    fn sizes_count_nested_entries_and_string_bytes() {
        let inner: Array = (0..60).map(Dynamic::from_int).collect();
        let mut m = Map::new();
        m.insert("a".into(), Dynamic::from_array(inner.clone()));
        m.insert("b".into(), Dynamic::from_array(inner));
        m.insert("s".into(), Dynamic::from("x".repeat(100)));
        let v = Dynamic::from_map(m);
        let s = Sizes::of(&v, usize::MAX);
        assert_eq!(
            s,
            Sizes {
                arrays: 120,
                maps: 3,
                strings: 100
            }
        );
        assert!(!s.within_caps());
        assert_eq!(s.read_cost(), 123 + 2);
        // The walk stops early, and a shared value is read through.
        assert!(Sizes::of(&v, 10).arrays < 120);
        assert_eq!(Sizes::of(&v.into_shared(), usize::MAX), s);
        let small = Sizes::of(&Dynamic::from("abc"), usize::MAX);
        assert!(small.within_caps());
        assert_eq!(small.read_cost(), 0);
        assert_eq!(Sizes::of(&Dynamic::from_int(1), 0), Sizes::default());
    }
}
