//! # framelite-document — a generic undoable document
//!
//! [`Doc<S>`] is the single source of truth for an app's state `S`. State changes go
//! through [`Command<S>`] values, which makes every mutation **named, fallible, and
//! undoable**: a command is applied to a *clone* first, so if it returns `Err` the live
//! state is untouched; on success the previous state is pushed onto an undo stack.
//!
//! The crate is domain-free on purpose — the app brings the state type and the commands.
//! That is what keeps the core reusable: a counter, a document tree, a scene graph all use
//! the same `Doc` / `History` machinery.

use serde::{Deserialize, Serialize};

/// A named, fallible mutation of state `S`. Implemented by the app for each edit.
pub trait Command<S> {
    /// Human / log name of the command.
    fn name(&self) -> &str;
    /// Apply the change. Return `Err(msg)` to reject it — the document stays unchanged.
    fn apply(&self, state: &mut S) -> Result<(), String>;
}

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

/// The application document: state `S` behind an undo/redo [`History`], mutated only by
/// dispatching [`Command<S>`]s.
#[derive(Clone, Debug)]
pub struct Doc<S> {
    history: History<S>,
}

/// Default undo depth when none is given.
pub const DEFAULT_MAX_DEPTH: usize = 100;

impl<S: Clone> Doc<S> {
    /// New document with the default undo depth.
    pub fn new(initial: S) -> Self {
        Self::with_depth(initial, DEFAULT_MAX_DEPTH)
    }

    /// New document with an explicit undo depth.
    pub fn with_depth(initial: S, max_depth: usize) -> Self {
        Self {
            history: History::new(initial, max_depth),
        }
    }

    /// The current state.
    pub fn state(&self) -> &S {
        self.history.present()
    }

    /// Apply a command transactionally: it runs against a clone, and only a successful
    /// result is committed to history. A rejected command leaves the document untouched.
    pub fn dispatch<C: Command<S>>(&mut self, cmd: &C) -> Result<(), String> {
        let mut next = self.history.present().clone();
        cmd.apply(&mut next)?;
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

/// The undo/redo verbs, as a serializable value an app can route over the bus and turn
/// into [`Doc::undo`] / [`Doc::redo`] calls.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BuiltInCommand {
    Undo,
    Redo,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Add(i64);
    impl Command<i64> for Add {
        fn name(&self) -> &str {
            "add"
        }
        fn apply(&self, state: &mut i64) -> Result<(), String> {
            *state += self.0;
            Ok(())
        }
    }

    struct MustBePositive(i64);
    impl Command<i64> for MustBePositive {
        fn name(&self) -> &str {
            "must-be-positive"
        }
        fn apply(&self, state: &mut i64) -> Result<(), String> {
            let next = *state + self.0;
            if next < 0 {
                return Err("would go negative".into());
            }
            *state = next;
            Ok(())
        }
    }

    #[test]
    fn dispatch_then_undo_redo() {
        let mut doc = Doc::new(0i64);
        doc.dispatch(&Add(5)).unwrap();
        doc.dispatch(&Add(3)).unwrap();
        assert_eq!(*doc.state(), 8);

        assert!(doc.undo());
        assert_eq!(*doc.state(), 5);
        assert!(doc.redo());
        assert_eq!(*doc.state(), 8);
    }

    #[test]
    fn rejected_command_leaves_state_and_history_untouched() {
        let mut doc = Doc::new(2i64);
        let err = doc.dispatch(&MustBePositive(-10)).unwrap_err();
        assert_eq!(err, "would go negative");
        assert_eq!(*doc.state(), 2);
        assert!(!doc.can_undo());
    }

    #[test]
    fn new_dispatch_clears_redo() {
        let mut doc = Doc::new(0i64);
        doc.dispatch(&Add(1)).unwrap();
        doc.undo();
        assert!(doc.can_redo());
        doc.dispatch(&Add(9)).unwrap();
        assert!(!doc.can_redo());
        assert_eq!(*doc.state(), 9);
    }
}
