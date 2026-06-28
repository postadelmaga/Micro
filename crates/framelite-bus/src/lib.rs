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
//! ## Retained channels (the generic "replay state")
//! A channel marked stateful with [`LocalBus::retain`] keeps its **last** envelope and
//! replays it to any *new* subscriber, so a module that joins late immediately learns the
//! current value (like a count). Unmarked channels are transient events — nothing is
//! replayed, so a one-shot (a tick, a shutdown) never spuriously re-fires.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Mutex;

pub use framelite_protocol::{Channel, Envelope, ModuleId};

/// Bus error as a message — matches the project's lightweight error style.
pub type BusError = String;

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
}

// --- the in-process broker -----------------------------------------------------

struct Inner {
    /// channel name → the live subscriber inboxes on it.
    subs: HashMap<Channel, Vec<mpsc::Sender<Envelope>>>,
    /// channel name → its last envelope, for channels marked stateful.
    retained: HashMap<Channel, Envelope>,
    /// channels whose last value is kept and replayed to new subscribers.
    stateful: std::collections::HashSet<Channel>,
}

/// In-process pub/sub broker. Cheap to clone behind an `Arc`; all methods take `&self`.
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
                stateful: std::collections::HashSet::new(),
            }),
        }
    }

    /// Mark a channel as stateful: its last published envelope is kept and replayed to any
    /// subscriber that joins afterwards. Call before publishing for the value to be retained.
    pub fn retain(&self, channel: impl Into<Channel>) {
        self.inner.lock().unwrap().stateful.insert(channel.into());
    }

    /// Publish an envelope to every current subscriber of its channel. On a stateful channel
    /// the envelope also becomes the retained value handed to future subscribers.
    pub fn publish(&self, env: Envelope) -> Result<(), BusError> {
        let mut inner = self.inner.lock().map_err(|e| e.to_string())?;
        let channel = env.channel.clone();
        if inner.stateful.contains(&channel) {
            inner.retained.insert(channel.clone(), env.clone());
        }
        if let Some(list) = inner.subs.get_mut(&channel) {
            // Deliver, dropping any subscriber whose receiver has gone away.
            list.retain(|tx| tx.send(env.clone()).is_ok());
        }
        Ok(())
    }

    /// Subscribe to a single channel.
    pub fn subscribe(&self, channel: impl Into<Channel>) -> Box<dyn Receiver> {
        self.subscribe_many([channel.into()])
    }

    /// Subscribe to several channels through **one** merged inbox: envelopes from any of the
    /// named channels arrive on the same receiver, in send order. A retained value on any of
    /// them is delivered immediately, newest-marked first is not guaranteed across channels.
    pub fn subscribe_many(
        &self,
        channels: impl IntoIterator<Item = Channel>,
    ) -> Box<dyn Receiver> {
        let (tx, rx) = mpsc::channel();
        let mut inner = self.inner.lock().unwrap();
        for channel in channels {
            // Hand the joiner the current value of a stateful channel right away.
            if let Some(env) = inner.retained.get(&channel).cloned() {
                let _ = tx.send(env);
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
        // Nothing else queued.
        assert!(tick.try_recv().unwrap().is_none());
    }

    #[test]
    fn retained_channel_replays_last_value_to_late_subscriber() {
        let bus = LocalBus::new();
        bus.retain("count");
        bus.publish(env("store", "count", 41)).unwrap();
        bus.publish(env("store", "count", 42)).unwrap();

        // Subscribed *after* the publishes — still sees the latest, exactly once.
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
        // Should not error even though the only subscriber is gone.
        bus.publish(env("a", "tick", 1)).unwrap();
    }
}
