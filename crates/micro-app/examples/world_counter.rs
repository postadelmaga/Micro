//! `cargo run -p micro-app --example world_counter`
//!
//! The `sources → world → sinks` spine in miniature, with no UI or devices:
//! * a **source** (`Ticker`) publishes a few counter actions and exits,
//! * the **world** ([`WorldModule`](micro_app::WorldModule), wired by [`App::world`])
//!   reduces them and republishes the counter on a retained `state` channel,
//! * a **sink** (`Printer`) renders every state it sees.
//!
//! Nothing here references anything else: the three talk only through the bus.

use std::thread::sleep;
use std::time::Duration;

use micro_app::App;
use micro_core::{Module, ModuleCtx};
use micro_document::Doc;
use micro_protocol::ModuleId;
use serde::{Deserialize, Serialize};

const ACTIONS: &str = "actions";
const STATE: &str = "state";

#[derive(Serialize, Deserialize, Clone, Debug)]
enum CounterAction {
    Add(i64),
    Sub(i64),
}

fn reduce(state: &mut i64, action: &CounterAction) -> Result<(), String> {
    match action {
        CounterAction::Add(n) => *state += n,
        CounterAction::Sub(n) => {
            if *state - n < 0 {
                return Err("would go negative".into());
            }
            *state -= n;
        }
    }
    Ok(())
}

/// A source: emits a fixed script of actions onto the bus, then returns.
struct Ticker;

impl Module for Ticker {
    fn id(&self) -> ModuleId {
        ModuleId::new("ticker")
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        for action in [
            CounterAction::Add(10),
            CounterAction::Add(5),
            CounterAction::Sub(100), // rejected by the reducer → world ignores it
            CounterAction::Sub(3),
        ] {
            if ctx.should_stop() {
                return;
            }
            println!("  ticker → {action:?}");
            let _ = ctx.publish_msg(ACTIONS, &action);
            sleep(Duration::from_millis(40));
        }
    }
}

/// A sink: prints the world's state every time it changes.
struct Printer;

impl Module for Printer {
    fn id(&self) -> ModuleId {
        ModuleId::new("printer")
    }

    fn subscriptions(&self) -> Vec<micro_protocol::Channel> {
        vec![STATE.into()]
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        while !ctx.should_stop() {
            if let Ok(Some(env)) = ctx.recv_timeout(Duration::from_millis(50)) {
                if let Ok(value) = env.decode::<i64>() {
                    println!("printer ← state = {value}");
                }
            }
        }
    }
}

fn main() {
    let mut app = App::new();
    app.world("world", ACTIONS, STATE, Doc::new(0i64, reduce));
    app.sink(Printer);
    app.source(Ticker);

    // Let the script play out, then wind everything down cleanly.
    sleep(Duration::from_millis(400));
    let report = app.shutdown_and_join();
    if !report.is_clean() {
        eprintln!("modules panicked: {:?}", report.panicked);
    }
}
