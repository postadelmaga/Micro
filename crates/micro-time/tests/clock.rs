//! The Clock source on a real runtime: it publishes monotonically-numbered ticks on the bus
//! at roughly the configured rate.

use std::thread::sleep;
use std::time::Duration;

use micro_core::Runtime;
use micro_time::{Clock, Tick};

#[test]
fn clock_publishes_increasing_ticks_at_its_rate() {
    let mut rt = Runtime::new();
    let bus = rt.bus();
    let rx = bus.subscribe("tick");

    // 100 Hz for ~120 ms → on the order of a dozen ticks; assert a robust lower bound.
    rt.spawn(Clock::hz("clock", "tick", 100.0));
    sleep(Duration::from_millis(120));
    rt.shutdown();
    rt.join();

    let mut ticks: Vec<Tick> = Vec::new();
    while let Ok(Some(env)) = rx.try_recv() {
        ticks.push(env.decode::<Tick>().unwrap());
    }

    assert!(ticks.len() >= 5, "expected several ticks, got {}", ticks.len());
    // Sequence numbers are 1,2,3,… and each dt is a positive elapsed time.
    for (i, t) in ticks.iter().enumerate() {
        assert_eq!(t.seq, i as u64 + 1);
        assert!(t.dt > 0.0);
        assert!(t.elapsed > 0.0);
    }
}
