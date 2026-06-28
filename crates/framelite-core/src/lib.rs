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
//!
//! ## Receive fast, compute on the pool, publish back (no head-of-line blocking)
//! A module runs on a *single* thread: if its `run` loop does heavy CPU work inline it
//! stops draining its inbox, and with the bounded bus later messages may be dropped. The
//! cure is to keep the loop cheap and hand the heavy work to the runtime's shared
//! [worker pool](Runtime) via [`ModuleCtx::offload`]. The job runs on a pool thread — not
//! the module's — and publishes its result back onto a channel through a captured
//! [`ModuleCtx::bus`] handle, which the module (or anyone else) then receives normally:
//!
//! ```no_run
//! # use framelite_core::{Module, ModuleCtx};
//! # use framelite_protocol::ModuleId;
//! # use std::time::Duration;
//! # struct Worker;
//! # impl Module for Worker {
//! #     fn id(&self) -> ModuleId { ModuleId::new("worker") }
//! fn run(self: Box<Self>, ctx: ModuleCtx) {
//!     while !ctx.should_stop() {
//!         match ctx.recv_timeout(Duration::from_millis(50)) {
//!             Ok(Some(job)) => {
//!                 // Receive fast: don't block the loop on the heavy part.
//!                 let bus = ctx.bus();
//!                 let me = ctx.id().clone();
//!                 ctx.offload(move || {
//!                     let answer = expensive(&job); // runs on a pool thread
//!                     let _ = bus.publish(
//!                         framelite_protocol::Envelope::new(me, "result", answer),
//!                     );
//!                 });
//!             }
//!             Ok(None) => {}
//!             Err(_) => break,
//!         }
//!     }
//! }
//! # }
//! # fn expensive(_e: &framelite_protocol::Envelope) -> serde_json::Value { serde_json::Value::Null }
//! ```

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use framelite_bus::{LocalBus, Receiver};
use serde::Serialize;
pub use framelite_bus::{BusError, LocalBus as Bus};
pub use framelite_protocol::{Channel, Envelope, ModuleId, Topic};

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

// --- worker pool (offload heavy work off a module's receive loop) ---------------

/// A boxed unit of CPU work to run on the pool.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// Cheap-to-clone submit handle to the runtime's shared worker pool. A [`ModuleCtx`] holds
/// one so a module can [`offload`](ModuleCtx::offload) heavy work. When every clone *and*
/// the pool's own handle are dropped the job queue closes — which is exactly how the
/// [`Runtime`] tears the pool down without a hang.
#[derive(Clone)]
struct PoolHandle {
    tx: mpsc::Sender<Job>,
}

impl PoolHandle {
    fn submit(&self, job: Job) {
        // If the pool is already shutting down (workers gone), the job is simply dropped.
        let _ = self.tx.send(job);
    }
}

/// A fixed set of worker threads draining one shared job queue. Owned by the [`Runtime`]
/// (not public surface). Sized from [`std::thread::available_parallelism`] with a sane
/// fallback, it lets modules push CPU work off their single receive thread so their inbox
/// keeps draining. Shutdown is deterministic: drop the submit handle so the queue closes,
/// then join the workers — no leaked or detached threads.
struct WorkerPool {
    /// The submit side cloned into every module's [`ModuleCtx`]. `Option` so
    /// [`WorkerPool::shutdown`] can drop the pool's own copy to help close the queue.
    handle: Option<PoolHandle>,
    workers: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    fn new() -> Self {
        let size = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .max(1);
        let (tx, rx) = mpsc::channel::<Job>();
        // One receiver shared by all workers: a worker locks only to dequeue, then runs the
        // job with the lock released so peers can pull the next one concurrently.
        let rx = Arc::new(Mutex::new(rx));
        let mut workers = Vec::with_capacity(size);
        for i in 0..size {
            let rx = rx.clone();
            let handle = std::thread::Builder::new()
                .name(format!("framelite-worker-{i}"))
                .spawn(move || loop {
                    let job = {
                        let guard = rx.lock().unwrap();
                        guard.recv()
                    };
                    match job {
                        // A panicking job must not take the worker (or the pool) down.
                        Ok(job) => {
                            let _ = std::panic::catch_unwind(AssertUnwindSafe(job));
                        }
                        // Every sender dropped: the queue is closed, so this worker exits.
                        Err(_) => break,
                    }
                })
                .expect("failed to spawn worker thread");
            workers.push(handle);
        }
        Self {
            handle: Some(PoolHandle { tx }),
            workers,
        }
    }

    /// A cheap clone of the submit handle for a module's [`ModuleCtx`].
    fn handle(&self) -> PoolHandle {
        self.handle
            .clone()
            .expect("worker pool used after shutdown")
    }

    /// Close the queue and join every worker. Idempotent. Callers must have dropped every
    /// other [`PoolHandle`] clone first (the [`Runtime`] joins module threads beforehand,
    /// which drops their [`ModuleCtx`] and thus their handle) so `recv` actually returns.
    fn shutdown(&mut self) {
        self.handle = None; // drop the pool's own sender → workers see the channel close.
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What a module gets to talk to the world: its identity, a handle to publish, the merged
/// inbox of its subscribed channels, the shared shutdown signal, and a handle to the
/// runtime's worker pool for offloading heavy work off its receive loop.
pub struct ModuleCtx {
    id: ModuleId,
    bus: Arc<LocalBus>,
    rx: Box<dyn Receiver>,
    shutdown: Shutdown,
    pool: PoolHandle,
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

    /// Publish a typed message on a [`Topic<T>`] — the compiler-checked form of
    /// [`publish_msg`](Self::publish_msg): the topic fixes both the channel *and* the payload
    /// type, so a producer can't send the wrong shape on the wrong channel.
    pub fn publish_on<T: Serialize>(&self, topic: &Topic<T>, msg: &T) -> Result<(), BusError> {
        self.bus.publish(topic.encode(self.id.clone(), msg)?)
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

    /// Run `job` on the runtime's shared **worker pool** instead of this module's thread,
    /// so the receive loop keeps draining its inbox while the heavy work happens.
    ///
    /// This is the *receive fast, compute on the pool, publish back* pattern: the loop does
    /// the cheap part and hands the expensive part here; the job captures a [`bus`](Self::bus)
    /// handle and publishes its result back onto a channel when it finishes (see the crate
    /// docs). The job runs concurrently with — and outlives, if need be — the call, so it
    /// must own everything it touches (`Send + 'static`); it cannot borrow the module's state.
    ///
    /// If the runtime is already shutting down (the pool is gone) the job is silently dropped.
    pub fn offload<F: FnOnce() + Send + 'static>(&self, job: F) {
        self.pool.submit(Box::new(job));
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

/// The in-process host: owns the [`LocalBus`], the [`Shutdown`] signal, the shared worker
/// pool, and the threads of the modules it spawned.
///
/// The pool exists so a module never has to choose between draining its inbox and doing
/// heavy work: a module's `run` stays a tight receive loop and pushes CPU-bound jobs onto
/// the pool with [`ModuleCtx::offload`] (see the crate-level docs for the *receive fast,
/// compute on the pool, publish back* pattern). The pool is sized from
/// [`std::thread::available_parallelism`] and is shut down deterministically by
/// [`Runtime::join`] — after the modules stop, their handles are dropped and the workers
/// join, leaving no detached threads.
pub struct Runtime {
    bus: Arc<LocalBus>,
    shutdown: Shutdown,
    panicked: Arc<Mutex<Vec<ModuleId>>>,
    handles: Vec<(ModuleId, JoinHandle<()>)>,
    pool: WorkerPool,
    /// Count of module threads currently running — bumped on spawn, dropped when a module's
    /// `run` returns (cleanly or by panic). Cheap liveness for a status line.
    live: Arc<AtomicUsize>,
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
            pool: WorkerPool::new(),
            live: Arc::new(AtomicUsize::new(0)),
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

    /// How many spawned modules are still running. Drops to zero as modules finish (on
    /// shutdown or panic) — pair it with [`LocalBus::metrics`](framelite_bus::LocalBus::metrics)
    /// for a cheap health view of a running app.
    pub fn live_count(&self) -> usize {
        self.live.load(Ordering::Relaxed)
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
            pool: self.pool.handle(),
        };
        let boxed: Box<dyn Module> = Box::new(module);
        let panicked = self.panicked.clone();
        let shutdown = self.shutdown.clone();
        let id_for_thread = id.clone();
        let live = self.live.clone();
        live.fetch_add(1, Ordering::Relaxed);
        let handle = std::thread::Builder::new()
            .name(id.0.clone())
            .spawn(move || {
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| boxed.run(ctx)));
                if result.is_err() {
                    panicked.lock().unwrap().push(id_for_thread);
                    shutdown.trigger();
                }
                // This module is no longer running, however it ended.
                live.fetch_sub(1, Ordering::Relaxed);
            })
            .expect("failed to spawn module thread");
        self.handles.push((id, handle));
    }

    /// Wait for every spawned module to finish, returning which ones panicked. Modules exit
    /// when the bus closes or when they observe [`Runtime::shutdown`].
    pub fn join(mut self) -> JoinReport {
        for (_, handle) in self.handles.drain(..) {
            let _ = handle.join();
        }
        // Modules are done → their `ModuleCtx` (and thus their pool handles) are dropped.
        // Now the only remaining sender is the pool's own: closing it lets the workers
        // finish the queue and exit, so the join below cannot hang.
        self.pool.shutdown();
        let panicked = Arc::try_unwrap(self.panicked)
            .map(|m| m.into_inner().unwrap())
            .unwrap_or_default();
        JoinReport { panicked }
    }
}
