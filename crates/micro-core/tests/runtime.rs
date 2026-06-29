//! End-to-end: two modules drive a shared document over the bus, plus lifecycle checks.
//!
//! A `Ticker` (pure producer) emits N typed increments on `tick`. A `Store` owns a
//! `Doc<i64, CounterAction>`, applies an `Add` action per tick, and publishes the new value
//! on the retained `count` channel. Shutdown is first-class (no ad-hoc control channel).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use micro_bus::LocalBus;
use micro_core::{Module, ModuleCtx, Runtime};
use micro_document::Doc;
use micro_protocol::{Channel, ModuleId};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Tick {
    amount: i64,
}

#[derive(Serialize, Deserialize)]
struct Count {
    value: i64,
}

#[derive(Serialize, Deserialize)]
enum CounterAction {
    Add(i64),
}

fn reduce(state: &mut i64, action: &CounterAction) -> Result<(), String> {
    match action {
        CounterAction::Add(n) => {
            *state += n;
            Ok(())
        }
    }
}

struct Ticker {
    ticks: i64,
}
impl Module for Ticker {
    fn id(&self) -> ModuleId {
        ModuleId::new("ticker")
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        for _ in 0..self.ticks {
            ctx.publish_msg("tick", &Tick { amount: 1 }).unwrap();
        }
    }
}

struct Store {
    doc: Doc<i64, CounterAction>,
}
impl Module for Store {
    fn id(&self) -> ModuleId {
        ModuleId::new("store")
    }
    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new("tick")]
    }
    fn run(mut self: Box<Self>, ctx: ModuleCtx) {
        while !ctx.should_stop() {
            match ctx.recv_timeout(Duration::from_millis(50)) {
                Ok(Some(env)) => {
                    if let Ok(tick) = env.decode::<Tick>() {
                        self.doc.dispatch(&CounterAction::Add(tick.amount)).unwrap();
                        ctx.publish_msg("count", &Count { value: *self.doc.state() })
                            .unwrap();
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    }
}

#[test]
fn modules_drive_a_document_over_the_bus() {
    let bus = Arc::new(LocalBus::new());
    bus.retain("count");
    let mut rt = Runtime::with_bus(bus.clone());

    let counts = bus.subscribe("count");

    rt.spawn(Store { doc: Doc::new(0, reduce) });
    rt.spawn(Ticker { ticks: 5 });

    let mut seen = Vec::new();
    while seen.len() < 5 {
        let env = counts.recv().unwrap();
        seen.push(env.decode::<Count>().unwrap().value);
    }
    assert_eq!(seen, vec![1, 2, 3, 4, 5]);

    rt.shutdown();
    let report = rt.join();
    assert!(report.is_clean(), "no module should have panicked");

    // The retained `count` channel re-syncs a subscriber that joins after the work is done.
    let late = bus.subscribe("count");
    assert_eq!(late.recv().unwrap().decode::<Count>().unwrap().value, 5);
}

/// A long-lived module observes `should_stop` and exits when the runtime asks it to.
struct Idle {
    ran: Arc<AtomicBool>,
}
impl Module for Idle {
    fn id(&self) -> ModuleId {
        ModuleId::new("idle")
    }
    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new("nothing")]
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        self.ran.store(true, Ordering::Release);
        while !ctx.should_stop() {
            let _ = ctx.recv_timeout(Duration::from_millis(10));
        }
    }
}

#[test]
fn shutdown_stops_a_long_lived_module() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut rt = Runtime::new();
    rt.spawn(Idle { ran: ran.clone() });

    // Give it a moment to start, then ask it to stop; join must return.
    while !ran.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    rt.shutdown();
    let report = rt.join();
    assert!(report.is_clean());
}

/// A module that panics must be isolated, recorded, and trigger fail-fast shutdown.
struct Bomb;
impl Module for Bomb {
    fn id(&self) -> ModuleId {
        ModuleId::new("bomb")
    }
    fn run(self: Box<Self>, _ctx: ModuleCtx) {
        panic!("boom");
    }
}

#[test]
fn module_panic_is_isolated_and_reported() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut rt = Runtime::new();
    let sig = rt.shutdown_signal();

    rt.spawn(Bomb);
    // A peer that runs until the bomb's panic triggers fail-fast shutdown.
    rt.spawn(Idle { ran: ran.clone() });

    let report = rt.join();
    assert_eq!(report.panicked, vec![ModuleId::new("bomb")]);
    assert!(sig.is_triggered(), "panic should have triggered shutdown");
}
