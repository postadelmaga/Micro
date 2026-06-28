//! # framelite-core — the module runtime
//!
//! This crate is the micro-kernel: it composes [`framelite_bus`] and [`framelite_document`]
//! into a tiny app shell. A [`Module`] declares the channels it listens on and a `run` loop;
//! the [`Runtime`] subscribes it to the bus and spawns it on its own thread. Modules talk
//! only through their [`ModuleCtx`] (publish + receive) — never directly to each other,
//! which is exactly what keeps them swappable.
//!
//! Only an **in-process** host is shipped here. Because modules are written against the bus
//! traits, a sidecar or remote transport could be added later without changing a module.
//!
//! ```no_run
//! use framelite_core::{Module, ModuleCtx, Runtime};
//! use framelite_protocol::{Channel, ModuleId};
//!
//! struct Logger;
//! impl Module for Logger {
//!     fn id(&self) -> ModuleId { ModuleId::new("logger") }
//!     fn subscriptions(&self) -> Vec<Channel> { vec![Channel::new("tick")] }
//!     fn run(self: Box<Self>, ctx: ModuleCtx) {
//!         while let Ok(env) = ctx.recv() {
//!             println!("{} -> {}", env.channel, env.payload);
//!         }
//!     }
//! }
//!
//! let mut rt = Runtime::new();
//! rt.spawn(Logger);
//! rt.bus().publish(framelite_protocol::Envelope::new(
//!     ModuleId::new("main"), "tick", serde_json::json!({ "n": 1 }))).unwrap();
//! ```

use std::sync::Arc;
use std::thread::JoinHandle;

use framelite_bus::{LocalBus, Receiver};
pub use framelite_bus::{BusError, LocalBus as Bus};
pub use framelite_protocol::{Channel, Envelope, ModuleId};

/// What a module gets to talk to the world: its identity, a handle to publish, and the
/// merged inbox of the channels it subscribed to.
pub struct ModuleCtx {
    id: ModuleId,
    bus: Arc<LocalBus>,
    rx: Box<dyn Receiver>,
}

impl ModuleCtx {
    /// This module's id.
    pub fn id(&self) -> &ModuleId {
        &self.id
    }

    /// Publish a payload on `channel`, stamped with this module's id as the sender.
    pub fn publish(
        &self,
        channel: impl Into<Channel>,
        payload: serde_json::Value,
    ) -> Result<(), BusError> {
        self.bus
            .publish(Envelope::new(self.id.clone(), channel, payload))
    }

    /// Block for the next envelope on a subscribed channel (`Err` once the bus closes).
    pub fn recv(&self) -> Result<Envelope, BusError> {
        self.rx.recv()
    }

    /// Non-blocking poll of the inbox.
    pub fn try_recv(&self) -> Result<Option<Envelope>, BusError> {
        self.rx.try_recv()
    }

    /// A clone of the bus handle, for modules that need to publish outside `run`'s loop.
    pub fn bus(&self) -> Arc<LocalBus> {
        self.bus.clone()
    }
}

/// A unit of behaviour hosted by the [`Runtime`]. Implement `run` as the module's loop;
/// it owns its state and ends when it returns (typically when `recv` reports the bus is
/// closed, or on a shutdown message).
pub trait Module: Send + 'static {
    /// Stable id used as the `from` of everything this module publishes.
    fn id(&self) -> ModuleId;

    /// Channels this module wants delivered to its [`ModuleCtx`] inbox. Default: none
    /// (a pure producer).
    fn subscriptions(&self) -> Vec<Channel> {
        Vec::new()
    }

    /// The module's run loop, on its own thread.
    fn run(self: Box<Self>, ctx: ModuleCtx);
}

/// The in-process host: owns the [`LocalBus`] and the threads of the modules it spawned.
pub struct Runtime {
    bus: Arc<LocalBus>,
    handles: Vec<(ModuleId, JoinHandle<()>)>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// New runtime with a fresh bus.
    pub fn new() -> Self {
        Self::with_bus(Arc::new(LocalBus::new()))
    }

    /// New runtime over an existing bus — useful when the caller wants to publish or
    /// subscribe from outside any module (e.g. the app's main thread).
    pub fn with_bus(bus: Arc<LocalBus>) -> Self {
        Self {
            bus,
            handles: Vec::new(),
        }
    }

    /// The shared bus handle.
    pub fn bus(&self) -> Arc<LocalBus> {
        self.bus.clone()
    }

    /// Subscribe a module to its channels and start it on a new thread.
    pub fn spawn<M: Module>(&mut self, module: M) {
        let id = module.id();
        let rx = self.bus.subscribe_many(module.subscriptions());
        let ctx = ModuleCtx {
            id: id.clone(),
            bus: self.bus.clone(),
            rx,
        };
        let boxed: Box<dyn Module> = Box::new(module);
        let handle = std::thread::Builder::new()
            .name(id.0.clone())
            .spawn(move || boxed.run(ctx))
            .expect("failed to spawn module thread");
        self.handles.push((id, handle));
    }

    /// Wait for every spawned module to finish. Modules typically exit when the bus closes
    /// (all `Sender`s dropped) or on a shutdown message they listen for.
    pub fn join(self) {
        for (_, handle) in self.handles {
            let _ = handle.join();
        }
    }
}
