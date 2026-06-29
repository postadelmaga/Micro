//! The **world**: the single stateful node of the `sources → world → sinks` spine.
//!
//! [`WorldModule`] is the bridge between [`micro_document::Doc`] (transactional,
//! undoable state) and the bus. It subscribes to an *actions* channel, dispatches each
//! action through the document's reducer, and — only when a dispatch commits — republishes
//! the new state on a *state* channel. Because the state channel is **retained** (the
//! [`App`](crate::App) marks it so), a sink that subscribes late immediately re-syncs to the
//! current state instead of waiting for the next change.

use std::time::Duration;

use micro_core::{Module, ModuleCtx};
use micro_document::Doc;
use micro_protocol::{Channel, ModuleId};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// A [`Module`] that owns a [`Doc<S, A>`] and exposes it on the bus as the world node:
/// actions in on one channel, state snapshots out on another.
///
/// Build one directly and hand it to [`App::spawn`](crate::App::spawn), or — preferred —
/// let [`App::world`](crate::App::world) construct and wire it (it also marks the state
/// channel retained, which is the invariant that makes late sinks re-sync).
pub struct WorldModule<S, A> {
    id: ModuleId,
    actions: Channel,
    state: Channel,
    doc: Doc<S, A>,
}

impl<S, A> WorldModule<S, A> {
    /// New world node: `actions` is the channel it reduces, `state` the channel it
    /// republishes the document's state on after every committed action.
    pub fn new(
        id: impl Into<String>,
        actions: impl Into<Channel>,
        state: impl Into<Channel>,
        doc: Doc<S, A>,
    ) -> Self {
        Self {
            id: ModuleId::new(id),
            actions: actions.into(),
            state: state.into(),
            doc,
        }
    }

    /// The channel this world republishes its state on (callers may want to subscribe a
    /// sink to it, or mark it retained themselves when not using [`App::world`](crate::App::world)).
    pub fn state_channel(&self) -> &Channel {
        &self.state
    }
}

impl<S, A> Module for WorldModule<S, A>
where
    S: Serialize + Clone + Send + 'static,
    A: DeserializeOwned + 'static,
{
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn subscriptions(&self) -> Vec<Channel> {
        vec![self.actions.clone()]
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let mut doc = self.doc;
        let state = self.state;

        // Publish the initial snapshot so a sink that subscribes later (the state channel is
        // retained by the App) re-syncs to the starting state immediately.
        let _ = ctx.publish_msg(state.clone(), doc.state());

        while !ctx.should_stop() {
            match ctx.recv_timeout(Duration::from_millis(50)) {
                Ok(Some(env)) => {
                    // An envelope on the actions channel whose payload isn't our action shape
                    // is simply ignored — the world only knows how to reduce `A`.
                    if let Ok(action) = env.decode::<A>() {
                        // Transactional: a rejected action leaves the document untouched and
                        // republishes nothing, so sinks only ever see committed state.
                        if doc.dispatch(&action).is_ok() {
                            let _ = ctx.publish_msg(state.clone(), doc.state());
                        }
                    }
                }
                Ok(None) => {}
                // The bus closed: nothing more can arrive, so the world is done.
                Err(_) => break,
            }
        }
    }
}
