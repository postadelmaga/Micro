//! Minimal framelite app: `cargo run -p framelite-core --example counter`
//!
//! Shows the whole core working together — a bus, an undoable document, and two modules
//! that never reference each other, only the channels `tick` / `count` / `control`.

use std::sync::Arc;

use framelite_bus::LocalBus;
use framelite_core::{Module, ModuleCtx, Runtime};
use framelite_document::{Command, Doc};
use framelite_protocol::{Channel, ModuleId};

/// The one edit our document supports.
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

/// Pure producer: emit a handful of increments, then ask everyone to shut down.
struct Ticker;
impl Module for Ticker {
    fn id(&self) -> ModuleId {
        ModuleId::new("ticker")
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        for _ in 0..5 {
            ctx.publish("tick", serde_json::json!({ "amount": 1 })).unwrap();
        }
        ctx.publish("control", serde_json::json!({ "shutdown": true }))
            .unwrap();
    }
}

/// Owns the document; turns ticks into commands and republishes the running total.
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
                    let total = *self.doc.state();
                    ctx.publish("count", serde_json::json!({ "value": total })).unwrap();
                }
                "control" if env.payload["shutdown"].as_bool() == Some(true) => break,
                _ => {}
            }
        }
    }
}

fn main() {
    let bus = Arc::new(LocalBus::new());
    bus.retain("count"); // count is durable state: late joiners get the latest value
    let mut rt = Runtime::with_bus(bus.clone());

    let counts = bus.subscribe("count");

    rt.spawn(Store { doc: Doc::new(0) });
    rt.spawn(Ticker);

    for _ in 0..5 {
        let env = counts.recv().unwrap();
        println!("count = {}", env.payload["value"]);
    }

    rt.join();
    println!("done.");
}
