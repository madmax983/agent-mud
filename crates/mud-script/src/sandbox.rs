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
//! - `eval`, `print` and `debug` are disabled; floats, time, modules and
//!   custom syntax are compiled out (see the workspace `Cargo.toml`).
//! - Every call runs under `catch_unwind`: a panic is a fault.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};

use mud_world::hooks::WorldDice;
use rhai::packages::{ArithmeticPackage, BasicIteratorPackage, LogicPackage, Package};
use rhai::{
    AST, CallFnOptions, Dynamic, Engine, EvalAltResult, NativeCallContext, Position, Scope,
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

/// Runs `f` on the call state of `ctx`.
///
/// # Errors
/// `f` fails, or the call has no state.
pub fn with_state<R>(
    ctx: &NativeCallContext,
    f: impl FnOnce(&mut CallState) -> Result<R, String>,
) -> RhaiResult<R> {
    let handle = ctx
        .tag()
        .and_then(|t| t.clone().try_cast::<StateHandle>())
        .ok_or_else(|| fault("this function needs a script call"))?;
    let mut state = handle.0.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut state).map_err(fault)
}

/// Charges `n` work units.
///
/// # Errors
/// The call has done more work than its limit.
pub fn charge(ctx: &NativeCallContext, n: usize) -> RhaiResult<()> {
    with_state(ctx, |s| {
        s.work = s.work.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if s.work > s.work_limit {
            Err(format!("the script did too much work ({} units)", s.work))
        } else {
            Ok(())
        }
    })
}

/// The sandboxed engine.
#[must_use]
pub fn engine(limits: &Limits) -> Engine {
    let mut e = Engine::new_raw();
    ArithmeticPackage::new().register_into_engine(&mut e);
    LogicPackage::new().register_into_engine(&mut e);
    BasicIteratorPackage::new().register_into_engine(&mut e);
    for symbol in ["eval", "print", "debug"] {
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
