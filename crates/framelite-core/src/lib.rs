//! # framelite-core — the module runtime
//!
//! This crate is the micro-kernel: it composes [`framelite_bus`] and [`framelite_document`]
//! into a tiny app shell. A [`Module`] declares the channels it listens on and a `run` loop;
//! the [`Runtime`] subscribes it to the bus and spawns it on its own thread. Modules talk
//! only through their [`ModuleCtx`] (publish + receive) — never directly to each other,
//! which is exactly what keeps them swappable.
//!
//! ## Lifecycle & supervision (thread model, no async)
//! The runtime owns a cooperative [`Shutdown`] signal. A module's loop checks
//! [`ModuleCtx::should_stop`] and blocks with [`ModuleCtx::recv_timeout`] so it wakes
//! periodically to observe it. [`Runtime::shutdown`] flips the signal; [`Runtime::join`]
//! waits and reports which modules **panicked**. Supervision is *fail-fast*: a module that
//! panics is isolated to its own thread, recorded, and automatically triggers shutdown of
//! the others — the app winds down cleanly instead of hanging. (Heartbeat/auto-restart is a
//! deliberate non-goal here; that is the parent Frame's territory.)
//!
//! Only an **in-process** host is shipped. Because modules are written against the bus
//! traits, a sidecar or remote transport could be added later without changing a module.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use framelite_bus::{LocalBus, Receiver};
use serde::Serialize;
pub use framelite_bus::{BusError, LocalBus as Bus};
pub use framelite_protocol::{Channel, Envelope, ModuleId};

/// A cooperative shutdown signal shared by the runtime and every module. Cheap to clone.
#[derive(Clone)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    /// Whether shutdown has been requested.
    pub fn is_triggered(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    /// Request shutdown. Idempotent.
    pub fn trigger(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// What a module gets to talk to the world: its identity, a handle to publish, the merged
/// inbox of its subscribed channels, and the shared shutdown signal.
pub struct ModuleCtx {
    id: ModuleId,
    bus: Arc<LocalBus>,
    rx: Box<dyn Receiver>,
    shutdown: Shutdown,
}

impl ModuleCtx {
    /// This module's id.
    pub fn id(&self) -> &ModuleId {
        &self.id
    }

    /// Whether the runtime has asked everyone to stop. A module's loop should check this.
    pub fn should_stop(&self) -> bool {
        self.shutdown.is_triggered()
    }

    /// Publish a raw JSON payload on `channel`, stamped with this module's id.
    pub fn publish(
        &self,
        channel: impl Into<Channel>,
        payload: serde_json::Value,
    ) -> Result<(), BusError> {
        self.bus
            .publish(Envelope::new(self.id.clone(), channel, payload))
    }

    /// Publish a **typed** message on `channel` (serialized to the payload). Preferred over
    /// [`ModuleCtx::publish`]: the contract is a real Rust type, not hand-built JSON.
    pub fn publish_msg(
        &self,
        channel: impl Into<Channel>,
        msg: &impl Serialize,
    ) -> Result<(), BusError> {
        self.bus
            .publish(Envelope::encode(self.id.clone(), channel, msg)?)
    }

    /// Block for the next envelope on a subscribed channel (`Err` once the bus closes).
    pub fn recv(&self) -> Result<Envelope, BusError> {
        self.rx.recv()
    }

    /// Non-blocking poll of the inbox.
    pub fn try_recv(&self) -> Result<Option<Envelope>, BusError> {
        self.rx.try_recv()
    }

    /// Block for at most `timeout` (`Ok(None)` on timeout) — the loop-friendly receive that
    /// lets a module re-check [`ModuleCtx::should_stop`] without busy-spinning.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<Envelope>, BusError> {
        self.rx.recv_timeout(timeout)
    }

    /// A clone of the bus handle, for publishing outside `run`'s loop.
    pub fn bus(&self) -> Arc<LocalBus> {
        self.bus.clone()
    }
}

/// A unit of behaviour hosted by the [`Runtime`]. Implement `run` as the module's loop;
/// it owns its state and ends when it returns (on shutdown, or when `recv` reports the bus
/// is closed).
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

/// What [`Runtime::join`] reports after every module has finished.
#[derive(Debug, Default)]
pub struct JoinReport {
    /// Modules whose `run` panicked (isolated to their own thread; they triggered shutdown).
    pub panicked: Vec<ModuleId>,
}

impl JoinReport {
    pub fn is_clean(&self) -> bool {
        self.panicked.is_empty()
    }
}

/// The in-process host: owns the [`LocalBus`], the [`Shutdown`] signal, and the threads of
/// the modules it spawned.
pub struct Runtime {
    bus: Arc<LocalBus>,
    shutdown: Shutdown,
    panicked: Arc<Mutex<Vec<ModuleId>>>,
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
            shutdown: Shutdown::new(),
            panicked: Arc::new(Mutex::new(Vec::new())),
            handles: Vec::new(),
        }
    }

    /// The shared bus handle.
    pub fn bus(&self) -> Arc<LocalBus> {
        self.bus.clone()
    }

    /// A clone of the shutdown signal, e.g. to hand to code outside a module.
    pub fn shutdown_signal(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Ask every module to stop (cooperative — modules observe it via `should_stop`).
    pub fn shutdown(&self) {
        self.shutdown.trigger();
    }

    /// Subscribe a module to its channels and start it on a new thread. If its `run` panics,
    /// the panic is caught, the module is recorded, and shutdown is triggered (fail-fast).
    pub fn spawn<M: Module>(&mut self, module: M) {
        let id = module.id();
        let rx = self.bus.subscribe_many(module.subscriptions());
        let ctx = ModuleCtx {
            id: id.clone(),
            bus: self.bus.clone(),
            rx,
            shutdown: self.shutdown.clone(),
        };
        let boxed: Box<dyn Module> = Box::new(module);
        let panicked = self.panicked.clone();
        let shutdown = self.shutdown.clone();
        let id_for_thread = id.clone();
        let handle = std::thread::Builder::new()
            .name(id.0.clone())
            .spawn(move || {
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| boxed.run(ctx)));
                if result.is_err() {
                    panicked.lock().unwrap().push(id_for_thread);
                    shutdown.trigger();
                }
            })
            .expect("failed to spawn module thread");
        self.handles.push((id, handle));
    }

    /// Wait for every spawned module to finish, returning which ones panicked. Modules exit
    /// when the bus closes or when they observe [`Runtime::shutdown`].
    pub fn join(self) -> JoinReport {
        for (_, handle) in self.handles {
            let _ = handle.join();
        }
        let panicked = Arc::try_unwrap(self.panicked)
            .map(|m| m.into_inner().unwrap())
            .unwrap_or_default();
        JoinReport { panicked }
    }
}
