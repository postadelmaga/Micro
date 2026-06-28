//! # framelite-protocol — the wire types every module shares
//!
//! Zero logic, just the data a message needs to be routed: who sent it ([`ModuleId`]),
//! the named [`Channel`] it rides, and the [`Envelope`] that carries the payload. Routing
//! is **by channel** — a module subscribes to a channel and never inspects another
//! module's internals. That is what lets a module be written once and hosted anywhere
//! (in-process today; a sidecar or remote transport could be added without touching the
//! module's code).
//!
//! Unlike the parent *Frame* project, channels here are **free-form strings**, not a fixed
//! enum: framelite is the generic core, so an app declares whatever channels it needs
//! (`"tick"`, `"count"`, `"control"`, …) without editing this crate.

use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Identifies a module (a message's sender) on the bus.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModuleId(pub String);

impl ModuleId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for ModuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A named channel modules publish to / receive from. Free-form so adding a "topic" is
/// just using a new name — no transport code changes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Channel(pub String);

impl Channel {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl From<&str> for Channel {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for Channel {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a channel behaves for a *fresh* subscriber. The bus uses this to decide whether a
/// channel's last message may be **replayed** to a new subscriber: replaying durable state
/// re-syncs the joiner; replaying a transient event would spuriously re-fire it.
///
/// In framelite the kind is not baked into the channel name — an app marks the stateful
/// channels on the bus (see `LocalBus::retain`). This enum names the two intents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    /// The latest value *is* the truth; replaying it re-syncs a fresh module (e.g. a count).
    State,
    /// One-shot occurrences; replaying repeats them wrongly (e.g. a tick, a shutdown).
    Event,
}

/// An addressed message on the bus: who sent it, on which channel, and the payload. The
/// channel tells the receiver how to interpret `payload`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub from: ModuleId,
    pub channel: Channel,
    pub payload: serde_json::Value,
}

impl Envelope {
    pub fn new(from: ModuleId, channel: impl Into<Channel>, payload: serde_json::Value) -> Self {
        Self {
            from,
            channel: channel.into(),
            payload,
        }
    }

    /// Build an envelope by serializing a **typed** message into the JSON payload. This is
    /// the typed seam over the generic bus: producers send a real Rust type instead of
    /// hand-built JSON, so a field rename is a compile error, not a silent `null`.
    pub fn encode(
        from: ModuleId,
        channel: impl Into<Channel>,
        msg: &impl Serialize,
    ) -> Result<Self, String> {
        let payload = serde_json::to_value(msg).map_err(|e| e.to_string())?;
        Ok(Self::new(from, channel, payload))
    }

    /// Deserialize the payload into a typed message `T`. The receiver names the type it
    /// expects (`env.decode::<Tick>()`) instead of indexing into a `serde_json::Value`.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_value(self.payload.clone()).map_err(|e| e.to_string())
    }
}

/// A **typed** view of a channel: a channel name bound, at the type level, to the payload
/// type `T` that rides on it. It carries no data of `T` (just a name), so it is `Send + Sync
/// + Clone` for *any* `T`, and it makes the bus contract checkable by the compiler:
/// [`encode`](Topic::encode) only accepts a `&T`, [`decode`](Topic::decode) only yields a
/// `T`, so a producer and a consumer that share a `Topic<T>` can never disagree on the shape.
///
/// The bus stays string-routed underneath — `Topic<T>` is a zero-cost ergonomic skin, not a
/// new transport. Declare app topics once and pass them around:
///
/// ```
/// # use framelite_protocol::{Topic, ModuleId};
/// # use serde::{Serialize, Deserialize};
/// #[derive(Serialize, Deserialize, PartialEq, Debug)]
/// struct Tick { n: i64 }
/// let tick: Topic<Tick> = Topic::new("tick");
/// let env = tick.encode(ModuleId::new("clock"), &Tick { n: 1 }).unwrap();
/// assert_eq!(tick.decode(&env).unwrap(), Tick { n: 1 });
/// ```
pub struct Topic<T> {
    channel: Channel,
    /// `fn() -> T` so the marker is `Send + Sync` and variance is sound regardless of `T`.
    _marker: PhantomData<fn() -> T>,
}

impl<T> Topic<T> {
    /// Bind a channel name to the payload type `T`.
    pub fn new(channel: impl Into<Channel>) -> Self {
        Self {
            channel: channel.into(),
            _marker: PhantomData,
        }
    }

    /// The underlying channel — for `subscribe`/`retain`/`overflow`, which are name-based.
    pub fn channel(&self) -> &Channel {
        &self.channel
    }
}

impl<T: Serialize> Topic<T> {
    /// Build an envelope carrying a `T` on this topic's channel, stamped with `from`.
    pub fn encode(&self, from: ModuleId, msg: &T) -> Result<Envelope, String> {
        Envelope::encode(from, self.channel.clone(), msg)
    }
}

impl<T: DeserializeOwned> Topic<T> {
    /// Decode an envelope's payload as this topic's `T`. (The caller is expected to only pass
    /// envelopes from this topic's channel; the type is what the topic guarantees.)
    pub fn decode(&self, env: &Envelope) -> Result<T, String> {
        env.decode::<T>()
    }
}

/// Cloneable for any `T`: only the channel name is held.
impl<T> Clone for Topic<T> {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for Topic<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Topic({})", self.channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_from_str_and_string_match() {
        assert_eq!(Channel::from("tick"), Channel::new("tick"));
        assert_eq!(Channel::from("tick".to_owned()), Channel::new("tick"));
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let env = Envelope::new(
            ModuleId::new("ticker"),
            "tick",
            serde_json::json!({ "n": 1 }),
        );
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.from, ModuleId::new("ticker"));
        assert_eq!(back.channel, Channel::new("tick"));
        assert_eq!(back.payload, serde_json::json!({ "n": 1 }));
    }

    #[test]
    fn encode_decode_round_trips_a_typed_message() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Tick {
            amount: i64,
        }
        let env = Envelope::encode(ModuleId::new("ticker"), "tick", &Tick { amount: 3 }).unwrap();
        let back: Tick = env.decode().unwrap();
        assert_eq!(back, Tick { amount: 3 });

        // Wrong shape surfaces as an error, not a silent default.
        #[derive(Deserialize)]
        struct Other {
            #[allow(dead_code)]
            name: String,
        }
        assert!(env.decode::<Other>().is_err());
    }

    #[test]
    fn topic_round_trips_and_targets_its_channel() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Tick {
            n: i64,
        }
        let tick: Topic<Tick> = Topic::new("tick");
        let env = tick.encode(ModuleId::new("clock"), &Tick { n: 7 }).unwrap();
        assert_eq!(env.channel, Channel::new("tick"));
        assert_eq!(tick.decode(&env).unwrap(), Tick { n: 7 });
    }

    #[test]
    fn topic_is_clone_and_send_for_any_payload() {
        // A payload type that is itself neither Send nor Clone must not infect the Topic.
        #[allow(dead_code)]
        struct NotSendNotClone(std::rc::Rc<i32>);
        fn assert_send_clone<X: Send + Clone>(_: &X) {}
        let t: Topic<NotSendNotClone> = Topic::new("x");
        assert_send_clone(&t);
        assert_eq!(t.clone().channel(), &Channel::new("x"));
    }
}
