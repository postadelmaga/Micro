//! The world node end-to-end: actions in → committed state out on a retained channel,
//! rejected actions change nothing.

use std::thread::sleep;
use std::time::Duration;

use micro_app::App;
use micro_document::Doc;
use micro_protocol::{Envelope, ModuleId};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
enum CounterAction {
    Add(i64),
    /// Rejected when it would take the counter negative.
    Sub(i64),
}

fn reduce(state: &mut i64, action: &CounterAction) -> Result<(), String> {
    match action {
        CounterAction::Add(n) => {
            *state += n;
            Ok(())
        }
        CounterAction::Sub(n) => {
            if *state - n < 0 {
                Err("would go negative".into())
            } else {
                *state -= n;
                Ok(())
            }
        }
    }
}

/// Poll the latest value on a state subscriber until it equals `want` or we give up.
fn await_state(rx: &dyn micro_bus::Receiver, want: i64) -> Option<i64> {
    let mut last = None;
    for _ in 0..400 {
        while let Ok(Some(env)) = rx.try_recv() {
            last = env.decode::<i64>().ok();
        }
        if last == Some(want) {
            return last;
        }
        sleep(Duration::from_millis(5));
    }
    last
}

#[test]
fn world_publishes_committed_state_and_ignores_rejected_actions() {
    let mut app = App::new();
    let bus = app.bus();

    // Spawning the world subscribes it to "actions" synchronously, so actions we publish
    // right after are delivered (not dropped) and the state channel is marked retained.
    app.world("world", "actions", "state", Doc::new(0i64, reduce));

    let state_rx = bus.subscribe("state");

    let publish = |a: &CounterAction| {
        let env = Envelope::encode(ModuleId::new("test"), "actions", a).unwrap();
        bus.publish(env).unwrap();
    };
    publish(&CounterAction::Add(5));
    publish(&CounterAction::Add(3));
    publish(&CounterAction::Sub(100)); // rejected: would go negative → no state change

    // Converges to 8 (5 + 3); the rejected Sub never moves it past 8.
    assert_eq!(await_state(state_rx.as_ref(), 8), Some(8));

    app.shutdown_and_join();
}

#[test]
fn late_subscriber_resyncs_to_retained_world_state() {
    let mut app = App::new();
    let bus = app.bus();
    app.world("world", "actions", "state", Doc::new(0i64, reduce));

    let env = Envelope::encode(ModuleId::new("test"), "actions", &CounterAction::Add(7)).unwrap();
    bus.publish(env).unwrap();

    // Give the world a moment to commit and republish, then subscribe *after* the change:
    // the retained state channel must replay the latest value (7) to this fresh subscriber.
    sleep(Duration::from_millis(50));
    let late = bus.subscribe("state");
    assert_eq!(await_state(late.as_ref(), 7), Some(7));

    app.shutdown_and_join();
}
