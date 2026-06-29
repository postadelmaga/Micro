//! # micro-input — the input source
//!
//! Input is the **source** end of micro's `sources → world → sinks` dataflow. A real UI
//! frontend (winit, egui, …) owns the OS event loop and pumps raw, device-specific events on
//! its main thread. This crate does *not* depend on any of those: it defines a small,
//! **device-neutral** [`InputEvent`] vocabulary and an [`InputMapper`] that turns those
//! events into the app's own **actions** and publishes them on the bus, so the `world` can
//! reduce them like any other message.
//!
//! ## Why no winit/egui dependency
//! Translating a concrete frontend's events (`winit::event::WindowEvent`, an egui `Event`, …)
//! into [`InputEvent`] is intentionally left to the **app**. Keeping that mapping outside the
//! framework means micro never pins a windowing stack or its version, an app can target
//! winit, egui, SDL, a test harness, or a replay log without this crate changing, and the
//! core stays dependency-light. The app writes a tiny `winit_event -> InputEvent` adapter
//! once; everything downstream speaks the neutral vocabulary.
//!
//! ## The shape of a mapping
//! An app declares an action type `A` and a [`Topic<A>`] for the channel those actions ride.
//! It builds an [`InputMapper`] with a closure `&InputEvent -> Option<A>`: returning `Some`
//! emits an action, returning `None` ignores the event. Ignoring is the common case — most
//! raw events (mouse moves, modifier-only keys, keys the app doesn't bind) map to nothing,
//! and that is **not** an error.
//!
//! ```
//! # use std::sync::Arc;
//! # use micro_bus::LocalBus;
//! # use micro_protocol::Topic;
//! # use micro_input::{InputEvent, InputMapper, Key};
//! # use serde::{Serialize, Deserialize};
//! #[derive(Serialize, Deserialize, PartialEq, Debug)]
//! enum Move { Left, Right }
//!
//! let bus = Arc::new(LocalBus::new());
//! let actions: Topic<Move> = Topic::new("move");
//! let mapper = InputMapper::new(bus.clone(), "input", actions, |ev| match ev {
//!     InputEvent::KeyDown(Key::Left) => Some(Move::Left),
//!     InputEvent::KeyDown(Key::Right) => Some(Move::Right),
//!     _ => None, // unbound events are silently ignored
//! });
//! mapper.feed(&InputEvent::KeyDown(Key::Left)).unwrap();
//! ```

use std::sync::Arc;

use micro_bus::{LocalBus, ModuleId};
use micro_protocol::Topic;
use serde::Serialize;

/// A device-neutral key. Deliberately a *small* set — printable characters arrive as
/// [`Key::Char`] (the frontend has already resolved layout/IME to a character), and the few
/// named keys are the ones an app commonly binds. An app that needs more extends its own
/// `winit -> InputEvent` adapter; the framework stays minimal.
#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Char(char),
    Escape,
    Enter,
    Space,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
}

/// A device-neutral mouse button.
#[derive(Clone, Debug, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// A device-neutral input event — the only vocabulary this crate exposes to the rest of the
/// app. Press/release are split into [`KeyDown`](InputEvent::KeyDown)/[`KeyUp`](InputEvent::KeyUp)
/// (and a `pressed` flag for the mouse) so a mapper can bind either edge, e.g. fire on key
/// *down* but stop on key *up*.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    KeyDown(Key),
    KeyUp(Key),
    MouseMoved { x: f64, y: f64 },
    MouseButton { button: MouseButton, pressed: bool },
    Wheel { delta: f32 },
}

/// The app's mapping policy: per event, an optional action to emit. Boxed so an
/// [`InputMapper`] has one concrete type whatever closure the app supplies; `Send + Sync` so
/// it can be shared across threads with the `Arc<LocalBus>`.
type MapFn<A> = Box<dyn Fn(&InputEvent) -> Option<A> + Send + Sync>;

/// Translates [`InputEvent`]s into app actions and publishes them on the bus.
///
/// It owns the three things needed to emit an action: the shared [`LocalBus`], the
/// [`ModuleId`] stamped on every envelope it sends (so consumers can see input is the
/// source), and the [`Topic<A>`] naming the channel + payload type of the actions. The
/// closure is the app's policy — the only domain knowledge here — kept boxed so a mapper has
/// one concrete type regardless of which closure an app supplies. `Send + Sync` bounds let
/// the mapper be shared across threads alongside the `Arc<LocalBus>`.
pub struct InputMapper<A> {
    bus: Arc<LocalBus>,
    id: ModuleId,
    topic: Topic<A>,
    map: MapFn<A>,
}

impl<A: Serialize> InputMapper<A> {
    /// Build a mapper. `id` is the source name stamped on published envelopes; `topic` binds
    /// the action channel to `A`; `map` decides, per event, whether (and which) action to emit.
    pub fn new(
        bus: Arc<LocalBus>,
        id: impl Into<String>,
        topic: Topic<A>,
        map: impl Fn(&InputEvent) -> Option<A> + Send + Sync + 'static,
    ) -> Self {
        Self {
            bus,
            id: ModuleId::new(id),
            topic,
            map: Box::new(map),
        }
    }

    /// The topic this mapper publishes on — for the `world` side to `subscribe`/`retain` the
    /// same channel without re-stating the name.
    pub fn topic(&self) -> &Topic<A> {
        &self.topic
    }

    /// Run one event through the mapping. If it yields an action, encode it on the topic
    /// (stamped with this mapper's id) and publish it; if it yields `None`, do nothing and
    /// succeed — an unmapped event is normal, not an error. The only `Err` is a genuine
    /// encode/publish failure, propagated as the project's `String` error.
    pub fn feed(&self, event: &InputEvent) -> Result<(), String> {
        match (self.map)(event) {
            Some(action) => {
                let env = self.topic.encode(self.id.clone(), &action)?;
                self.bus.publish(env)
            }
            None => Ok(()),
        }
    }

    /// Feed a batch of events in order — convenience for draining a frame's worth of events.
    /// Stops at the first publish error (none of which a pure mapping closure can cause).
    pub fn feed_all(&self, events: impl IntoIterator<Item = InputEvent>) -> Result<(), String> {
        for event in events {
            self.feed(&event)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    enum Move {
        Left,
        Right,
        Jump,
    }

    #[test]
    fn maps_bound_events_to_actions_and_ignores_the_rest() {
        let bus = Arc::new(LocalBus::new());
        let topic: Topic<Move> = Topic::new("move");
        let rx = bus.subscribe(topic.channel().clone());

        let mapper = InputMapper::new(bus.clone(), "input", topic, |ev| match ev {
            InputEvent::KeyDown(Key::Left) => Some(Move::Left),
            InputEvent::KeyDown(Key::Right) => Some(Move::Right),
            InputEvent::KeyDown(Key::Space) => Some(Move::Jump),
            _ => None,
        });

        // A mix of bound and unbound events; only three are bound.
        mapper
            .feed_all([
                InputEvent::KeyDown(Key::Left),
                InputEvent::MouseMoved { x: 1.0, y: 2.0 }, // unmapped → nothing
                InputEvent::KeyDown(Key::Right),
                InputEvent::KeyUp(Key::Left), // unmapped (only KeyDown is bound) → nothing
                InputEvent::KeyDown(Key::Space),
            ])
            .unwrap();

        // Exactly the mapped actions arrived, in order.
        assert_eq!(mapper_decode(rx.as_ref()), Some(Move::Left));
        assert_eq!(mapper_decode(rx.as_ref()), Some(Move::Right));
        assert_eq!(mapper_decode(rx.as_ref()), Some(Move::Jump));
        // …and nothing else: the unmapped events produced no envelopes.
        assert!(rx.try_recv().unwrap().is_none());
    }

    #[test]
    fn published_action_is_stamped_with_the_source_id() {
        let bus = Arc::new(LocalBus::new());
        let topic: Topic<Move> = Topic::new("move");
        let rx = bus.subscribe(topic.channel().clone());

        let mapper = InputMapper::new(bus.clone(), "kbd", topic, |_| Some(Move::Jump));
        mapper.feed(&InputEvent::Wheel { delta: 1.0 }).unwrap();

        let env = rx.try_recv().unwrap().unwrap();
        assert_eq!(env.from, ModuleId::new("kbd"));
        assert_eq!(env.decode::<Move>().unwrap(), Move::Jump);
    }

    /// Pull the next envelope (if any) off the receiver and decode it as a `Move`.
    fn mapper_decode(rx: &dyn micro_bus::Receiver) -> Option<Move> {
        rx.try_recv().unwrap().map(|env| env.decode::<Move>().unwrap())
    }
}
