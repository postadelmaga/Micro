//! # micro-app — the `sources → world → sinks` composition layer
//!
//! The micro kernel ([`micro_bus`], [`micro_core`], [`micro_document`]) is
//! deliberately generic: everything is just a [`Module`](micro_core::Module) talking over
//! string-named channels. This crate adds the one *opinion* that turns that kernel into a
//! framework — a single, documented dataflow spine:
//!
//! ```text
//! sources  ──actions──▶  world  ──state (retained)──▶  sinks
//! (input,               (Doc<S,A>)                      (video, audio, ui)
//!  midi-in,             the only stateful node          stateless, read the state
//!  clock, net)          dispatch + undo/redo            render it
//! ```
//!
//! * **Sources** publish *actions* (and events) onto the bus; they own no shared state.
//! * The **world** is the single stateful node: a [`WorldModule`] wraps a
//!   [`Doc<S,A>`](micro_document::Doc), applies each incoming action through the
//!   reducer, and republishes the new state on a **retained** channel.
//! * **Sinks** subscribe to the world's state (and events) and render it; they never mutate
//!   shared state and never talk to a source or another sink directly.
//!
//! [`App`] is a thin builder over [`Runtime`](micro_core::Runtime) that makes this shape
//! the path of least resistance: [`App::world`] spawns the world *and* marks its state
//! channel retained in one call, and [`App::source`] / [`App::sink`] are intent-named
//! spawns so a `main` reads like the diagram above. Nothing here is new transport — it is
//! all the existing kernel, wired with a convention.

mod app;
mod world;

pub use app::App;
pub use world::WorldModule;
