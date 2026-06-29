//! # micro-bridge — a bus over a byte stream
//!
//! A [`LocalBus`] routes in-process at channel granularity. Because modules are written
//! against the bus traits, the *same* module can have peers in another process — you just need
//! a transport that carries envelopes across the boundary and republishes them. This crate is
//! that transport: it (de)serializes [`Envelope`]s as length-prefixed JSON over any
//! [`Read`]/[`Write`] stream (a `TcpStream`, a pipe, a Unix socket), so a bus on one side and a
//! bus on the other behave like one.
//!
//! Two halves, deliberately one-directional so there is no echo to break:
//! * [`Bridge::egress`] subscribes to a set of channels on the local bus and writes every
//!   envelope to the stream — the *outbound* side.
//! * [`Bridge::ingress`] reads envelopes from the stream and republishes them on the local
//!   bus — the *inbound* side.
//!
//! A full duplex link is just two bridges with the channel sets chosen so a channel is never
//! forwarded both ways (which would loop). The framing helpers [`write_frame`]/[`read_frame`]
//! are public for anyone building a different topology.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use micro_bus::{Channel, Envelope, LocalBus};

/// How often an egress loop wakes to check for shutdown while idle.
const POLL: Duration = Duration::from_millis(100);

/// Write one envelope to `w` as a little-endian `u32` length prefix followed by its JSON.
pub fn write_frame(w: &mut impl Write, env: &Envelope) -> io::Result<()> {
    let bytes = serde_json::to_vec(env).map_err(io::Error::other)?;
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Read one framed envelope from `r`. Returns `Ok(None)` at a clean end of stream (the peer
/// closed), `Err` on a malformed frame or I/O error.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Envelope>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    let env = serde_json::from_slice(&buf).map_err(io::Error::other)?;
    Ok(Some(env))
}

/// A running one-directional link between a bus and a stream. Drop-safe: [`Bridge::stop`]
/// signals its thread and joins it.
pub struct Bridge {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Bridge {
    /// **Outbound**: forward every envelope published on `channels` of `bus` to `writer`.
    /// The loop wakes every [`POLL`] to observe [`Bridge::stop`], so it shuts down promptly
    /// even when the channels are idle.
    pub fn egress(
        bus: Arc<LocalBus>,
        channels: impl IntoIterator<Item = Channel>,
        mut writer: impl Write + Send + 'static,
    ) -> Self {
        let rx = bus.subscribe_many(channels);
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match rx.recv_timeout(POLL) {
                    Ok(Some(env)) => {
                        if write_frame(&mut writer, &env).is_err() {
                            break; // peer gone or stream broken
                        }
                    }
                    Ok(None) => {}        // idle tick: re-check stop
                    Err(_) => break,      // local bus closed
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// **Inbound**: read envelopes from `reader` and republish them on `bus`. The read blocks
    /// until a frame arrives or the peer closes; give `reader` a read timeout (e.g.
    /// `TcpStream::set_read_timeout`) if you need [`Bridge::stop`] to interrupt a quiet link
    /// promptly rather than at the next frame.
    pub fn ingress(bus: Arc<LocalBus>, mut reader: impl Read + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match read_frame(&mut reader) {
                    Ok(Some(env)) => {
                        let _ = bus.publish(env);
                    }
                    Ok(None) => break, // peer closed cleanly
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue, // read timeout
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => continue,
                    Err(_) => break,
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Signal the bridge's thread to stop and wait for it.
    pub fn stop(mut self) {
        self.signal_and_join();
    }

    fn signal_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.signal_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use micro_protocol::ModuleId;

    #[test]
    fn frame_round_trips_through_a_buffer() {
        let env = Envelope::new(ModuleId::new("a"), "x", serde_json::json!({ "n": 5 }));
        let mut buf = Vec::new();
        write_frame(&mut buf, &env).unwrap();
        let mut cursor = io::Cursor::new(buf);
        let back = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(back.channel, Channel::new("x"));
        assert_eq!(back.payload["n"], 5);
        // A second read hits clean EOF.
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }
}
