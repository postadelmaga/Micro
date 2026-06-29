//! The smallest possible micro module: `cargo run -p micro-core --example echo`
//!
//! One module, `Shout`, subscribes to the `say` channel, upper-cases the text, and
//! republishes it on `shout`. `main` plays the role of "the rest of the app": it publishes
//! a few lines and reads the results back — the two sides only ever share *channel names*.

use std::sync::Arc;
use std::time::Duration;

use micro_bus::LocalBus;
use micro_core::{Module, ModuleCtx, Runtime};
use micro_protocol::{Channel, ModuleId};
use serde::{Deserialize, Serialize};

/// Typed message carried on both channels — a field rename here is a compile error,
/// not a silent `null` in the JSON payload.
#[derive(Serialize, Deserialize)]
struct Line {
    text: String,
}

/// A single module: listen on `say`, shout back on `shout`.
struct Shout;

impl Module for Shout {
    fn id(&self) -> ModuleId {
        ModuleId::new("shout")
    }

    /// The only channels delivered to this module's inbox.
    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new("say")]
    }

    /// Tight receive loop: wake every 50ms to observe shutdown, otherwise process input.
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        while !ctx.should_stop() {
            match ctx.recv_timeout(Duration::from_millis(50)) {
                Ok(Some(env)) => {
                    let line: Line = match env.decode() {
                        Ok(l) => l,
                        Err(_) => continue, // ignore anything that isn't a Line
                    };
                    let shouted = Line { text: line.text.to_uppercase() };
                    ctx.publish_msg("shout", &shouted).unwrap();
                }
                Ok(None) => {}   // timeout → loop back and re-check should_stop
                Err(_) => break, // bus closed
            }
        }
    }
}

fn main() {
    let bus = Arc::new(LocalBus::new());
    let mut rt = Runtime::with_bus(bus.clone());

    // The app subscribes to the module's output *before* spawning it.
    let shouts = bus.subscribe("shout");
    rt.spawn(Shout);

    let lines = ["hello", "micro", "is tiny"];
    for text in lines {
        bus.publish(
            micro_protocol::Envelope::encode(
                ModuleId::new("main"),
                "say",
                &Line { text: text.to_string() },
            )
            .unwrap(),
        )
        .unwrap();
    }

    // Read back exactly as many results as we sent.
    for _ in lines {
        let env = shouts.recv().unwrap();
        let line: Line = env.decode().unwrap();
        println!("shout = {}", line.text);
    }

    rt.shutdown();
    let report = rt.join();
    println!("done (clean: {}).", report.is_clean());
}
