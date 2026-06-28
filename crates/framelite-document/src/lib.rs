//! # framelite-document — a generic undoable document (action + reducer)
//!
//! [`Doc<S, A>`] is the single source of truth for an app's state `S`. State changes are
//! described by **actions** `A` — plain, serializable values — and applied by a single
//! **reducer** `Fn(&mut S, &A) -> Result<(), String>`. Making mutations *data* (not trait
//! objects) is the robustness win: an action can be logged, replayed, persisted, or sent
//! across the bus, and the same action stream always reproduces the same state.
//!
//! Every dispatch is **transactional and undoable**: the reducer runs against a *clone*, so
//! a rejected action (`Err`) leaves the live state untouched; a successful one pushes the
//! previous state onto an undo stack.
//!
//! The crate is domain-free on purpose — the app brings the state type, the action enum, and
//! the reducer. A counter, a tree, a scene graph all reuse the same machinery.

use serde::{Deserialize, Serialize};

/// Generic undo/redo history for any cloneable state `S`. Holds the present value plus
/// bounded past/future stacks.
#[derive(Clone, Debug)]
pub struct History<S> {
    past: Vec<S>,
    present: S,
    future: Vec<S>,
    max_depth: usize,
}

impl<S: Clone> History<S> {
    pub fn new(initial: S, max_depth: usize) -> Self {
        Self {
            past: Vec::new(),
            present: initial,
            future: Vec::new(),
            max_depth,
        }
    }

    /// The current state.
    pub fn present(&self) -> &S {
        &self.present
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    /// Record `next` as the new present, pushing the old present onto the undo stack and
    /// clearing the redo stack. The undo stack is capped at `max_depth`.
    pub fn commit(&mut self, next: S) {
        self.past.push(std::mem::replace(&mut self.present, next));
        if self.past.len() > self.max_depth {
            self.past.remove(0);
        }
        self.future.clear();
    }

    /// Step back one state. Returns `false` if there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        if let Some(prev) = self.past.pop() {
            self.future.push(std::mem::replace(&mut self.present, prev));
            true
        } else {
            false
        }
    }

    /// Step forward one state. Returns `false` if there is nothing to redo.
    pub fn redo(&mut self) -> bool {
        if let Some(next) = self.future.pop() {
            self.past.push(std::mem::replace(&mut self.present, next));
            true
        } else {
            false
        }
    }
}

/// A reducer applied to state `S` by an action `A`. Returns `Err(msg)` to reject the
/// action — the document then stays unchanged.
pub type ReduceFn<S, A> = dyn Fn(&mut S, &A) -> Result<(), String> + Send + Sync;

/// Default undo depth when none is given.
pub const DEFAULT_MAX_DEPTH: usize = 100;

/// The application document: state `S` behind an undo/redo [`History`], mutated only by
/// dispatching serializable actions `A` through a fixed reducer.
pub struct Doc<S, A> {
    history: History<S>,
    reducer: Box<ReduceFn<S, A>>,
}

impl<S: Clone, A> Doc<S, A> {
    /// New document with the default undo depth and the given reducer.
    pub fn new(
        initial: S,
        reducer: impl Fn(&mut S, &A) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self::with_depth(initial, DEFAULT_MAX_DEPTH, reducer)
    }

    /// New document with an explicit undo depth.
    pub fn with_depth(
        initial: S,
        max_depth: usize,
        reducer: impl Fn(&mut S, &A) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            history: History::new(initial, max_depth),
            reducer: Box::new(reducer),
        }
    }

    /// The current state.
    pub fn state(&self) -> &S {
        self.history.present()
    }

    /// Apply an action transactionally: the reducer runs against a clone, and only a
    /// successful result is committed to history. A rejected action leaves the document
    /// untouched.
    pub fn dispatch(&mut self, action: &A) -> Result<(), String> {
        let mut next = self.history.present().clone();
        (self.reducer)(&mut next, action)?;
        self.history.commit(next);
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        self.history.undo()
    }

    pub fn redo(&mut self) -> bool {
        self.history.redo()
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }
}

/// The undo/redo verbs as a serializable action an app can route over the bus and turn into
/// [`Doc::undo`] / [`Doc::redo`] calls.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HistoryAction {
    Undo,
    Redo,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    enum CounterAction {
        Add(i64),
        /// Rejected when it would take the counter negative.
        Sub(i64),
    }

    fn reduce(state: &mut i64, action: &CounterAction) -> Result<(), String> {
        match action {
            CounterAction::Add(n) => {
                *state += n;
                Ok(())
            }
            CounterAction::Sub(n) => {
                if *state - n < 0 {
                    return Err("would go negative".into());
                }
                *state -= n;
                Ok(())
            }
        }
    }

    fn doc() -> Doc<i64, CounterAction> {
        Doc::new(0, reduce)
    }

    #[test]
    fn dispatch_then_undo_redo() {
        let mut d = doc();
        d.dispatch(&CounterAction::Add(5)).unwrap();
        d.dispatch(&CounterAction::Add(3)).unwrap();
        assert_eq!(*d.state(), 8);

        assert!(d.undo());
        assert_eq!(*d.state(), 5);
        assert!(d.redo());
        assert_eq!(*d.state(), 8);
    }

    #[test]
    fn rejected_action_leaves_state_and_history_untouched() {
        let mut d = doc();
        d.dispatch(&CounterAction::Add(2)).unwrap();
        let err = d.dispatch(&CounterAction::Sub(10)).unwrap_err();
        assert_eq!(err, "would go negative");
        assert_eq!(*d.state(), 2);
        // Only the successful Add is on the undo stack.
        assert!(d.undo());
        assert_eq!(*d.state(), 0);
        assert!(!d.can_undo());
    }

    #[test]
    fn new_dispatch_clears_redo() {
        let mut d = doc();
        d.dispatch(&CounterAction::Add(1)).unwrap();
        d.undo();
        assert!(d.can_redo());
        d.dispatch(&CounterAction::Add(9)).unwrap();
        assert!(!d.can_redo());
        assert_eq!(*d.state(), 9);
    }

    #[test]
    fn actions_are_data_replaying_reproduces_state() {
        // The whole point of action+reducer: a recorded action log replays to the same state.
        let log = vec![
            CounterAction::Add(10),
            CounterAction::Sub(4),
            CounterAction::Add(1),
        ];
        // Round-trip the log through JSON to prove actions are serializable data.
        let json = serde_json::to_string(&log).unwrap();
        let replayed: Vec<CounterAction> = serde_json::from_str(&json).unwrap();

        let mut d = doc();
        for a in &replayed {
            d.dispatch(a).unwrap();
        }
        assert_eq!(*d.state(), 7);
    }
}
