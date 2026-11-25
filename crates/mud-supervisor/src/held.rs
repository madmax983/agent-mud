//! The lock-order check (ADR 0002, ADR 0011).
//!
//! The order is `world → (router | state | wake book, one at a time) →
//! store cell`. `Game::act` and `Game::close_turn` take the world lock, so
//! a caller that holds a supervisor lock there inverts the order and can
//! deadlock against a world closure that takes that supervisor lock. Every
//! supervisor lock goes through [`lock`], which counts the locks this
//! thread holds; [`assert_none_held`] fails a debug build that calls into
//! the world with one held.

use std::cell::Cell;
use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, MutexGuard, PoisonError};

thread_local! {
    static HELD: Cell<u32> = const { Cell::new(0) };
}

/// A supervisor lock, counted while it is held.
#[derive(Debug)]
pub struct Held<'a, T>(MutexGuard<'a, T>);

impl<T> Deref for Held<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Held<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T> Drop for Held<'_, T> {
    fn drop(&mut self) {
        HELD.with(|h| h.set(h.get().saturating_sub(1)));
    }
}

/// Takes a supervisor lock (a poisoned one is taken all the same).
pub fn lock<T>(m: &Mutex<T>) -> Held<'_, T> {
    let guard = m.lock().unwrap_or_else(PoisonError::into_inner);
    HELD.with(|h| h.set(h.get().saturating_add(1)));
    Held(guard)
}

/// How many supervisor locks this thread holds.
pub fn held() -> u32 {
    HELD.with(Cell::get)
}

/// In a debug build, fails when this thread holds a supervisor lock: call
/// it right before a `Game` call that takes the world lock (`what`).
pub fn assert_none_held(what: &str) {
    debug_assert_eq!(
        held(),
        0,
        "a supervisor lock is held while {what} takes the world lock (ADR 0002)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_follows_the_guards() {
        let (a, b) = (Mutex::new(1), Mutex::new(2));
        assert_eq!(held(), 0);
        let ga = lock(&a);
        let mut gb = lock(&b);
        *gb += *ga;
        assert_eq!(held(), 2);
        drop(ga);
        assert_eq!(held(), 1);
        drop(gb);
        assert_eq!(held(), 0);
        assert_eq!(*lock(&b), 3);
        assert_none_held("a test");
    }
}
