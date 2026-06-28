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
}
