//! Minimal micro app: `cargo run -p micro-core --example counter`
//!
//! Shows the whole core working together — typed bus messages, an action+reducer document,
//! and two modules that never reference each other, only the channels `tick` / `count`.
//! Shutdown is first-class: `main` flips the runtime's signal, modules observe it.

use std::sync::Arc;
use std::time::Duration;

use micro_bus::LocalBus;
use micro_core::{Module, ModuleCtx, Runtime};
use micro_document::Doc;
use micro_protocol::{Channel, ModuleId};
use serde::{Deserialize, Serialize};

/// Typed message on the `tick` channel.
#[derive(Serialize, Deserialize)]
struct Tick {
    amount: i64,
}

/// Typed message on the `count` channel.
#[derive(Serialize, Deserialize)]
struct Count {
    value: i64,
}

/// The document's actions — plain serializable data, not trait objects.
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

/// Pure producer: emit a handful of increments.
struct Ticker;
impl Module for Ticker {
    fn id(&self) -> ModuleId {
        ModuleId::new("ticker")
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        for _ in 0..5 {
            ctx.publish_msg("tick", &Tick { amount: 1 }).unwrap();
        }
    }
}

/// Owns the document; turns ticks into actions and republishes the running total.
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
                    let tick: Tick = match env.decode() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    self.doc.dispatch(&CounterAction::Add(tick.amount)).unwrap();
                    ctx.publish_msg("count", &Count { value: *self.doc.state() })
                        .unwrap();
                }
                Ok(None) => {}      // timeout → re-check should_stop
                Err(_) => break,    // bus closed
            }
        }
    }
}

fn main() {
    let bus = Arc::new(LocalBus::new());
    bus.retain("count"); // count is durable state: late joiners get the latest value
    let mut rt = Runtime::with_bus(bus.clone());

    let counts = bus.subscribe("count");

    rt.spawn(Store { doc: Doc::new(0, reduce) });
    rt.spawn(Ticker);

    for _ in 0..5 {
        let env = counts.recv().unwrap();
        let count: Count = env.decode().unwrap();
        println!("count = {}", count.value);
    }

    rt.shutdown();
    let report = rt.join();
    println!("done (clean: {}).", report.is_clean());
}
