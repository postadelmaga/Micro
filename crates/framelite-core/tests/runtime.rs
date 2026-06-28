//! End-to-end: two modules drive a shared document over the bus.
//!
//! A `Ticker` (pure producer) emits N increments on the `tick` channel, then a shutdown on
//! `control`. A `Store` owns a `Doc<i64>`, applies an `Add` command per tick, and publishes
//! the new value on the retained `count` channel. The test asserts the values arrive in
//! order and that the channel's retention re-syncs a late subscriber.

use std::sync::Arc;

use framelite_bus::LocalBus;
use framelite_core::{Module, ModuleCtx, Runtime};
use framelite_document::{Command, Doc};
use framelite_protocol::{Channel, ModuleId};

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

struct Ticker {
    ticks: i64,
}
impl Module for Ticker {
    fn id(&self) -> ModuleId {
        ModuleId::new("ticker")
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        for _ in 0..self.ticks {
            ctx.publish("tick", serde_json::json!({ "amount": 1 })).unwrap();
        }
        ctx.publish("control", serde_json::json!({ "shutdown": true }))
            .unwrap();
    }
}

struct Store {
    doc: Doc<i64>,
}
impl Module for Store {
    fn id(&self) -> ModuleId {
        ModuleId::new("store")
    }
    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new("tick"), Channel::new("control")]
    }
    fn run(mut self: Box<Self>, ctx: ModuleCtx) {
        while let Ok(env) = ctx.recv() {
            match env.channel.0.as_str() {
                "tick" => {
                    let amount = env.payload["amount"].as_i64().unwrap_or(0);
                    self.doc.dispatch(&Add(amount)).unwrap();
                    ctx.publish("count", serde_json::json!({ "value": *self.doc.state() }))
                        .unwrap();
                }
                "control" if env.payload["shutdown"].as_bool() == Some(true) => break,
                _ => {}
            }
        }
    }
}

#[test]
fn modules_drive_a_document_over_the_bus() {
    let bus = Arc::new(LocalBus::new());
    bus.retain("count");
    let mut rt = Runtime::with_bus(bus.clone());

    // Subscribe before spawning so we capture every emission.
    let counts = bus.subscribe("count");

    rt.spawn(Store { doc: Doc::new(0) });
    rt.spawn(Ticker { ticks: 5 });

    let mut seen = Vec::new();
    while seen.len() < 5 {
        let env = counts.recv().unwrap();
        seen.push(env.payload["value"].as_i64().unwrap());
    }
    assert_eq!(seen, vec![1, 2, 3, 4, 5]);

    rt.join();

    // The retained `count` channel re-syncs a subscriber that joins after the work is done.
    let late = bus.subscribe("count");
    assert_eq!(late.recv().unwrap().payload["value"].as_i64().unwrap(), 5);
}
