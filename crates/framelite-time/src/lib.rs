//! # framelite-time — Clock source and Pacer
//!
//! Two ways to get cadence into the `sources → world → sinks` spine:
//!
//! * [`Clock`] is a **source module**: it publishes a [`Tick`] on a channel at a fixed rate,
//!   so a world (or several sinks) can share one heartbeat over the bus — e.g. a simulation
//!   that steps on every tick, or a UI that repaints on it.
//! * [`Pacer`] is a **self-driven frame limiter** a sink owns: call [`Pacer::tick`] at the
//!   top of a render loop and it sleeps to hold the target rate, returning the elapsed
//!   *delta time* so the sink can advance by real time, not by assumed frames.
//!
//! The two cover the two real shapes: a *pushed* cadence shared on the bus (Clock) and a
//! *pulled* cadence local to one loop (Pacer), the latter being what a video or audio sink
//! usually wants since its rate is dictated by the display or the device, not by the bus.

use std::thread;
use std::time::{Duration, Instant};

use framelite_core::{Module, ModuleCtx, ModuleId, Topic};
use serde::{Deserialize, Serialize};

/// The longest a `Clock` will sleep in one go, so it can observe shutdown promptly even at
/// very low tick rates.
const MAX_SLEEP: Duration = Duration::from_millis(20);

/// A single beat. `seq` counts from 1; `elapsed` is seconds since the clock started; `dt` is
/// seconds since the previous tick (the first tick's `dt` is the configured interval).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Tick {
    pub seq: u64,
    pub elapsed: f64,
    pub dt: f64,
}

/// A source module that publishes a [`Tick`] on a channel at a fixed interval. Wire it like
/// any source (`app.source(Clock::hz("clock", "tick", 60.0))`); subscribers reduce or render
/// on the beat.
pub struct Clock {
    id: ModuleId,
    topic: Topic<Tick>,
    interval: Duration,
}

impl Clock {
    /// A clock ticking `hz` times per second on `channel`.
    pub fn hz(id: impl Into<String>, channel: impl Into<framelite_core::Channel>, hz: f64) -> Self {
        let secs = if hz > 0.0 { 1.0 / hz } else { 0.0 };
        Self::every(id, channel, Duration::from_secs_f64(secs))
    }

    /// A clock ticking once per `interval` on `channel`.
    pub fn every(
        id: impl Into<String>,
        channel: impl Into<framelite_core::Channel>,
        interval: Duration,
    ) -> Self {
        Self {
            id: ModuleId::new(id),
            topic: Topic::new(channel),
            interval,
        }
    }

    /// The topic this clock publishes on (subscribe a sink/world to `topic.channel()`).
    pub fn topic(&self) -> &Topic<Tick> {
        &self.topic
    }
}

impl Module for Clock {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    // A clock listens to nothing — it is a pure source.

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let interval = self.interval;
        let start = Instant::now();
        let mut last = start;
        let mut next = start + interval;
        let mut seq = 0u64;

        while !ctx.should_stop() {
            let now = Instant::now();
            if now >= next {
                seq += 1;
                let tick = Tick {
                    seq,
                    elapsed: now.duration_since(start).as_secs_f64(),
                    dt: now.duration_since(last).as_secs_f64(),
                };
                last = now;
                let _ = ctx.publish_on(&self.topic, &tick);
                next += interval;
                // If we fell behind (a slow scheduler), resync instead of bursting catch-up
                // ticks forever.
                if next <= now {
                    next = now + interval;
                }
            } else {
                // Sleep until the next beat, but never so long we miss shutdown.
                thread::sleep((next - now).min(MAX_SLEEP));
            }
        }
    }
}

/// A self-driven frame limiter for a sink's loop. Construct it with a target rate, then call
/// [`tick`](Pacer::tick) once per iteration: it sleeps to the next boundary and hands back the
/// real delta time since the previous call.
pub struct Pacer {
    interval: Duration,
    last: Instant,
    next: Instant,
}

impl Pacer {
    /// A pacer holding `hz` iterations per second.
    pub fn hz(hz: f64) -> Self {
        let secs = if hz > 0.0 { 1.0 / hz } else { 0.0 };
        Self::every(Duration::from_secs_f64(secs))
    }

    /// A pacer with an explicit per-iteration interval.
    pub fn every(interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            interval,
            last: now,
            next: now + interval,
        }
    }

    /// Sleep until the next frame boundary, then return seconds elapsed since the previous
    /// `tick` (the loop's delta time). Resyncs without burst-catch-up if a frame ran long.
    pub fn tick(&mut self) -> f64 {
        let now = Instant::now();
        if now < self.next {
            thread::sleep(self.next - now);
        }
        let woke = Instant::now();
        let dt = woke.duration_since(self.last).as_secs_f64();
        self.last = woke;
        self.next += self.interval;
        if self.next <= woke {
            self.next = woke + self.interval;
        }
        dt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_holds_its_rate() {
        // 200 Hz → 5 ms/iter. Ten iterations should take clearly more than the time five
        // unpaced iterations would (≈0). Lower-bound only, to stay robust on busy CI.
        let mut pacer = Pacer::hz(200.0);
        let start = Instant::now();
        let mut total_dt = 0.0;
        for _ in 0..10 {
            total_dt += pacer.tick();
        }
        let wall = start.elapsed().as_secs_f64();
        assert!(wall >= 0.030, "10 iters @200Hz took only {wall}s");
        assert!(total_dt >= 0.030, "summed dt was only {total_dt}s");
    }

    #[test]
    fn pacer_dt_is_positive() {
        let mut pacer = Pacer::hz(1000.0);
        assert!(pacer.tick() > 0.0);
    }
}
