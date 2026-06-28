//! # framelite-bus — the abstract transport
//!
//! A module is written against the [`Sender`] / [`Receiver`] **traits** and never knows
//! who the peer is. The only transport framelite ships is [`LocalBus`]: an in-process
//! pub/sub broker over thread channels (zero serialization, ~ns latency). Because modules
//! depend on the traits, a future sidecar or network transport is *added* by implementing
//! them — existing module code does not change.
//!
//! ## Routing
//! [`LocalBus`] routes purely **by channel name**. A subscriber names the channels it
//! wants; a publisher names the channel it emits on. The broker fans each [`Envelope`] out
//! to every subscriber of its channel.
//!
//! ## Backpressure (bounded, non-blocking)
//! Each subscriber inbox is a **bounded** queue (default [`DEFAULT_CAPACITY`]). Publishing
//! never blocks the broker on a slow consumer: if a subscriber's inbox is full the envelope
//! is **dropped for that subscriber** and counted in [`LocalBus::dropped`]. This trades the
//! unbounded-memory risk for an explicit, observable loss under overload — never a silent
//! stall. Size the inbox with [`LocalBus::subscribe_with_capacity`] when a consumer must not
//! miss events.
//!
//! ## Retained channels (the generic "replay state")
//! A channel marked stateful with [`LocalBus::retain`] keeps its **last** envelope and
//! replays it to any *new* subscriber, so a module that joins late immediately learns the
//! current value (like a count). Unmarked channels are transient events — nothing is
//! replayed, so a one-shot (a tick) never spuriously re-fires.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

pub use framelite_protocol::{Channel, Envelope, ModuleId};

/// Bus error as a message — matches the project's lightweight error style.
pub type BusError = String;

/// Default bounded inbox depth per subscriber.
pub const DEFAULT_CAPACITY: usize = 1024;

/// Publishes envelopes onto the bus. `Send + Sync` so it can be shared across threads.
pub trait Sender: Send + Sync {
    fn send(&self, env: Envelope) -> Result<(), BusError>;
}

/// Receives envelopes from the bus.
pub trait Receiver: Send {
    /// Block until the next envelope (or the bus closes).
    fn recv(&self) -> Result<Envelope, BusError>;
    /// Non-blocking poll: `Ok(None)` when nothing is ready.
    fn try_recv(&self) -> Result<Option<Envelope>, BusError>;
    /// Block for at most `timeout`. `Ok(None)` on timeout — lets a module's loop wake
    /// periodically to check a shutdown flag without a busy spin.
    fn recv_timeout(&self, timeout: Duration) -> Result<Option<Envelope>, BusError>;
}

// --- the in-process broker -----------------------------------------------------

struct Inner {
    /// channel name → the live subscriber inboxes on it.
    subs: HashMap<Channel, Vec<mpsc::SyncSender<Envelope>>>,
    /// channel name → its last envelope, for channels marked stateful.
    retained: HashMap<Channel, Envelope>,
    /// channels whose last value is kept and replayed to new subscribers.
    stateful: HashSet<Channel>,
    /// total envelopes dropped because a subscriber inbox was full (observability).
    dropped: u64,
}

/// In-process pub/sub broker. Cheap to share behind an `Arc`; all methods take `&self`.
pub struct LocalBus {
    inner: Mutex<Inner>,
}

impl Default for LocalBus {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalBus {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                subs: HashMap::new(),
                retained: HashMap::new(),
                stateful: HashSet::new(),
                dropped: 0,
            }),
        }
    }

    /// Mark a channel as stateful: its last published envelope is kept and replayed to any
    /// subscriber that joins afterwards. Call before publishing for the value to be retained.
    pub fn retain(&self, channel: impl Into<Channel>) {
        self.inner.lock().unwrap().stateful.insert(channel.into());
    }

    /// Total number of envelopes dropped so far due to full subscriber inboxes.
    pub fn dropped(&self) -> u64 {
        self.inner.lock().unwrap().dropped
    }

    /// Publish an envelope to every current subscriber of its channel. Never blocks: a
    /// subscriber whose inbox is full has this envelope dropped (and counted); a subscriber
    /// whose receiver is gone is pruned. On a stateful channel the envelope also becomes the
    /// retained value handed to future subscribers.
    pub fn publish(&self, env: Envelope) -> Result<(), BusError> {
        let mut inner = self.inner.lock().map_err(|e| e.to_string())?;
        let channel = env.channel.clone();
        if inner.stateful.contains(&channel) {
            inner.retained.insert(channel.clone(), env.clone());
        }
        let dropped = if let Some(list) = inner.subs.get_mut(&channel) {
            let mut d = 0u64;
            let mut i = 0;
            while i < list.len() {
                match list[i].try_send(env.clone()) {
                    Ok(()) => i += 1,
                    // Full: drop for this subscriber, keep it subscribed.
                    Err(mpsc::TrySendError::Full(_)) => {
                        d += 1;
                        i += 1;
                    }
                    // Receiver gone: prune (swap_remove is fine, order across subs is unspecified).
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        list.swap_remove(i);
                    }
                }
            }
            d
        } else {
            0
        };
        inner.dropped += dropped;
        Ok(())
    }

    /// Subscribe to a single channel with the default inbox depth.
    pub fn subscribe(&self, channel: impl Into<Channel>) -> Box<dyn Receiver> {
        self.subscribe_many([channel.into()])
    }

    /// Subscribe to a single channel with an explicit inbox depth — raise it for a consumer
    /// that must not drop events under bursts.
    pub fn subscribe_with_capacity(
        &self,
        channel: impl Into<Channel>,
        capacity: usize,
    ) -> Box<dyn Receiver> {
        self.subscribe_inner([channel.into()], capacity)
    }

    /// Subscribe to several channels through **one** merged inbox: envelopes from any of the
    /// named channels arrive on the same receiver, in send order.
    pub fn subscribe_many(
        &self,
        channels: impl IntoIterator<Item = Channel>,
    ) -> Box<dyn Receiver> {
        self.subscribe_inner(channels, DEFAULT_CAPACITY)
    }

    fn subscribe_inner(
        &self,
        channels: impl IntoIterator<Item = Channel>,
        capacity: usize,
    ) -> Box<dyn Receiver> {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let mut inner = self.inner.lock().unwrap();
        for channel in channels {
            // Hand the joiner the current value of a stateful channel right away.
            if let Some(env) = inner.retained.get(&channel).cloned() {
                let _ = tx.try_send(env);
            }
            inner.subs.entry(channel).or_default().push(tx.clone());
        }
        Box::new(ChannelReceiver(rx))
    }
}

/// Lets an `Arc<LocalBus>` be used wherever a `&dyn Sender` is expected.
impl Sender for LocalBus {
    fn send(&self, env: Envelope) -> Result<(), BusError> {
        self.publish(env)
    }
}

struct ChannelReceiver(mpsc::Receiver<Envelope>);

impl Receiver for ChannelReceiver {
    fn recv(&self) -> Result<Envelope, BusError> {
        self.0.recv().map_err(|e| e.to_string())
    }
    fn try_recv(&self) -> Result<Option<Envelope>, BusError> {
        match self.0.try_recv() {
            Ok(env) => Ok(Some(env)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err("bus disconnected".into()),
        }
    }
    fn recv_timeout(&self, timeout: Duration) -> Result<Option<Envelope>, BusError> {
        match self.0.recv_timeout(timeout) {
            Ok(env) => Ok(Some(env)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("bus disconnected".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(from: &str, ch: &str, n: i64) -> Envelope {
        Envelope::new(ModuleId::new(from), ch, serde_json::json!({ "n": n }))
    }

    #[test]
    fn delivers_only_to_subscribers_of_the_channel() {
        let bus = LocalBus::new();
        let tick = bus.subscribe("tick");
        let _other = bus.subscribe("count");

        bus.publish(env("a", "tick", 1)).unwrap();

        let got = tick.recv().unwrap();
        assert_eq!(got.channel, Channel::new("tick"));
        assert_eq!(got.payload["n"], 1);
        assert!(tick.try_recv().unwrap().is_none());
    }

    #[test]
    fn retained_channel_replays_last_value_to_late_subscriber() {
        let bus = LocalBus::new();
        bus.retain("count");
        bus.publish(env("store", "count", 41)).unwrap();
        bus.publish(env("store", "count", 42)).unwrap();

        let late = bus.subscribe("count");
        let got = late.recv().unwrap();
        assert_eq!(got.payload["n"], 42);
        assert!(late.try_recv().unwrap().is_none());
    }

    #[test]
    fn unretained_channel_does_not_replay() {
        let bus = LocalBus::new();
        bus.publish(env("a", "tick", 1)).unwrap();
        let late = bus.subscribe("tick");
        assert!(late.try_recv().unwrap().is_none());
    }

    #[test]
    fn merged_inbox_receives_from_all_named_channels() {
        let bus = LocalBus::new();
        let rx = bus.subscribe_many([Channel::new("tick"), Channel::new("control")]);
        bus.publish(env("a", "tick", 1)).unwrap();
        bus.publish(env("a", "control", 9)).unwrap();

        let first = rx.recv().unwrap();
        let second = rx.recv().unwrap();
        let chans = [first.channel.0, second.channel.0];
        assert!(chans.contains(&"tick".to_string()));
        assert!(chans.contains(&"control".to_string()));
    }

    #[test]
    fn dropped_subscriber_is_pruned_on_publish() {
        let bus = LocalBus::new();
        let rx = bus.subscribe("tick");
        drop(rx);
        bus.publish(env("a", "tick", 1)).unwrap();
    }

    #[test]
    fn full_inbox_drops_and_counts_without_blocking() {
        let bus = LocalBus::new();
        // Capacity 2, never drained: the 3rd+ publishes are dropped, not blocked.
        let rx = bus.subscribe_with_capacity("tick", 2);
        for i in 0..5 {
            bus.publish(env("a", "tick", i)).unwrap();
        }
        assert_eq!(bus.dropped(), 3);
        // The two that fit are still there.
        assert!(rx.recv().unwrap().payload["n"].is_number());
        assert!(rx.recv().unwrap().payload["n"].is_number());
        assert!(rx.try_recv().unwrap().is_none());
    }

    #[test]
    fn recv_timeout_returns_none_then_value() {
        let bus = LocalBus::new();
        let rx = bus.subscribe("tick");
        assert!(rx.recv_timeout(Duration::from_millis(10)).unwrap().is_none());
        bus.publish(env("a", "tick", 7)).unwrap();
        let got = rx.recv_timeout(Duration::from_millis(10)).unwrap().unwrap();
        assert_eq!(got.payload["n"], 7);
    }
}
