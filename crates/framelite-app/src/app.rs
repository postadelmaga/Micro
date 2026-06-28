//! The [`App`] builder: declarative wiring for the `sources → world → sinks` spine.

use std::sync::Arc;

use framelite_bus::{LocalBus, Overflow};
use framelite_core::{JoinReport, Module, Runtime};
use framelite_document::Doc;
use framelite_protocol::Channel;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::WorldModule;

/// A thin builder over [`Runtime`] that wires modules and channels in one place, so an app's
/// `main` reads like the dataflow it implements.
///
/// The kernel already lets any [`Module`] publish/subscribe on any channel; `App` only adds
/// convenience and intent:
/// * [`App::retain`] / [`App::overflow`] declare channel behaviour up front (before anything
///   publishes), instead of scattering `bus.retain(...)` calls.
/// * [`App::source`] / [`App::sink`] are intent-named [`spawn`](App::spawn)s — identical at
///   runtime, but they make the wiring self-documenting.
/// * [`App::world`] builds a [`WorldModule`] from a [`Doc`] *and* marks its state channel
///   retained, enforcing the "world state is durable" invariant in a single call.
///
/// ```no_run
/// # use framelite_app::App;
/// # use framelite_document::Doc;
/// # use serde::{Serialize, Deserialize};
/// # #[derive(Serialize, Deserialize, Clone)] enum Action { Add(i64) }
/// # fn reduce(s: &mut i64, a: &Action) -> Result<(), String> { Ok(()) }
/// # fn build_source() -> impl framelite_core::Module { Src }
/// # fn build_sink() -> impl framelite_core::Module { Src }
/// # struct Src;
/// # impl framelite_core::Module for Src {
/// #   fn id(&self) -> framelite_core::ModuleId { framelite_core::ModuleId::new("x") }
/// #   fn run(self: Box<Self>, _: framelite_core::ModuleCtx) {}
/// # }
/// let mut app = App::new();
/// app.world("world", "actions", "state", Doc::new(0i64, reduce));
/// app.source(build_source());
/// app.sink(build_sink());
/// // ... run a UI / block on the main thread, holding `app.bus()` ...
/// let report = app.shutdown_and_join();
/// ```
pub struct App {
    bus: Arc<LocalBus>,
    rt: Runtime,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// New app with a fresh bus and runtime.
    pub fn new() -> Self {
        Self::with_bus(Arc::new(LocalBus::new()))
    }

    /// New app over an existing bus — useful when the caller (e.g. a UI on the main thread)
    /// must publish or subscribe from outside any module.
    pub fn with_bus(bus: Arc<LocalBus>) -> Self {
        let rt = Runtime::with_bus(bus.clone());
        Self { bus, rt }
    }

    /// A clone of the bus handle, for code outside a module (a UI, the main thread).
    pub fn bus(&self) -> Arc<LocalBus> {
        self.bus.clone()
    }

    /// Mark `channel` stateful: its last value is retained and replayed to late subscribers.
    /// Call before anything publishes on it (the world's state channel is handled for you by
    /// [`App::world`]).
    pub fn retain(&mut self, channel: impl Into<Channel>) -> &mut Self {
        self.bus.retain(channel);
        self
    }

    /// Set the overflow policy for a channel (see [`Overflow`]): `Block` for true
    /// backpressure on a channel that must not drop, `Drop` (the default) for real-time feeds.
    pub fn overflow(&mut self, channel: impl Into<Channel>, policy: Overflow) -> &mut Self {
        self.bus.set_overflow(channel, policy);
        self
    }

    /// Spawn any module on its own thread. [`source`](App::source) / [`sink`](App::sink) are
    /// intent-named aliases — use those at call sites to keep the wiring readable.
    pub fn spawn<M: Module>(&mut self, module: M) -> &mut Self {
        self.rt.spawn(module);
        self
    }

    /// Spawn a **source**: a module that produces actions/events onto the bus. (Alias of
    /// [`spawn`](App::spawn); the name documents the module's role in the dataflow.)
    pub fn source<M: Module>(&mut self, module: M) -> &mut Self {
        self.spawn(module)
    }

    /// Spawn a **sink**: a module that consumes state/events and renders or outputs them.
    /// (Alias of [`spawn`](App::spawn); the name documents the module's role.)
    pub fn sink<M: Module>(&mut self, module: M) -> &mut Self {
        self.spawn(module)
    }

    /// Spawn the **world** node: a [`WorldModule`] reducing `actions` into `doc`'s state and
    /// republishing it on `state`. The `state` channel is marked retained automatically — a
    /// world's state is durable by definition, so a late sink always re-syncs to it.
    pub fn world<S, A>(
        &mut self,
        id: impl Into<String>,
        actions: impl Into<Channel>,
        state: impl Into<Channel>,
        doc: Doc<S, A>,
    ) -> &mut Self
    where
        S: Serialize + Clone + Send + 'static,
        A: DeserializeOwned + 'static,
    {
        let state = state.into();
        self.bus.retain(state.clone());
        self.spawn(WorldModule::new(id, actions, state, doc))
    }

    /// How many spawned modules are still running — drops to zero as modules finish. Cheap
    /// liveness for a status overlay; pair with [`bus().metrics()`](framelite_bus::LocalBus::metrics).
    pub fn live_count(&self) -> usize {
        self.rt.live_count()
    }

    /// Ask every module to stop (cooperative — modules observe it via `should_stop`).
    pub fn shutdown(&self) {
        self.rt.shutdown();
    }

    /// Wait for every module to finish, reporting which ones panicked.
    pub fn join(self) -> JoinReport {
        self.rt.join()
    }

    /// Signal shutdown and then wait — the usual teardown after a blocking UI returns.
    pub fn shutdown_and_join(self) -> JoinReport {
        self.rt.shutdown();
        self.rt.join()
    }
}
