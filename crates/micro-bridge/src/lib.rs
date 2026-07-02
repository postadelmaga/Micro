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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Raw = 0,
    Json = 1,
}

impl TryFrom<u8> for FrameKind {
    type Error = String;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(FrameKind::Raw),
            1 => Ok(FrameKind::Json),
            v => Err(format!("Unknown FrameKind: {}", v)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFrame {
    pub channel: String,
    pub kind: FrameKind,
    pub data: Vec<u8>,
}

/// Write a raw frame to `w` using binary layout: length (u32), kind (u8), channel_len (u16), channel, data.
pub fn write_raw_frame(w: &mut impl Write, frame: &RawFrame) -> io::Result<()> {
    let channel_bytes = frame.channel.as_bytes();
    if channel_bytes.len() > u16::MAX as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Channel name too long"));
    }
    
    // total_len excludes total_len itself (4 bytes)
    // total_len = 1 (kind) + 2 (channel_len) + channel_bytes.len() + data.len()
    let total_len = 1 + 2 + channel_bytes.len() + frame.data.len();
    
    w.write_all(&(total_len as u32).to_le_bytes())?;
    w.write_all(&[frame.kind as u8])?;
    w.write_all(&(channel_bytes.len() as u16).to_le_bytes())?;
    w.write_all(channel_bytes)?;
    w.write_all(&frame.data)?;
    w.flush()
}

/// Read a raw frame from `r`. Returns `Ok(None)` on clean EOF, `Err` on I/O or malformed frame.
pub fn read_raw_frame(r: &mut impl Read) -> io::Result<Option<RawFrame>> {
    let mut total_len_bytes = [0u8; 4];
    match r.read_exact(&mut total_len_bytes) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    
    let total_len = u32::from_le_bytes(total_len_bytes) as usize;
    if total_len < 3 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Frame too small"));
    }
    
    let mut buffer = vec![0u8; total_len];
    r.read_exact(&mut buffer)?;
    
    let kind_byte = buffer[0];
    let kind = FrameKind::try_from(kind_byte)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        
    let channel_len = u16::from_le_bytes([buffer[1], buffer[2]]) as usize;
    if 3 + channel_len > total_len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid channel length"));
    }
    
    let channel_bytes = &buffer[3..3 + channel_len];
    let channel = String::from_utf8(channel_bytes.to_vec())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        
    let data = buffer[3 + channel_len..].to_vec();
    
    Ok(Some(RawFrame {
        channel,
        kind,
        data,
    }))
}

/// Write one envelope to `w` by serializing it as a JSON payload inside a `RawFrame`.
pub fn write_frame(w: &mut impl Write, env: &Envelope) -> io::Result<()> {
    let bytes = serde_json::to_vec(env).map_err(io::Error::other)?;
    let frame = RawFrame {
        channel: env.channel.0.clone(),
        kind: FrameKind::Json,
        data: bytes,
    };
    write_raw_frame(w, &frame)
}

/// Read one framed envelope from `r` by parsing the raw frame and decoding its JSON.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Envelope>> {
    match read_raw_frame(r)? {
        Some(frame) => {
            if frame.kind != FrameKind::Json {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Expected JSON frame, got {:?}", frame.kind),
                ));
            }
            let env = serde_json::from_slice(&frame.data).map_err(io::Error::other)?;
            Ok(Some(env))
        }
        None => Ok(None),
    }
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

    #[test]
    fn raw_frame_round_trips_through_a_buffer() {
        let frame = RawFrame {
            channel: "raw-pty-output".to_string(),
            kind: FrameKind::Raw,
            data: vec![0x01, 0x02, 0x03, 0xff, 0x00],
        };
        let mut buf = Vec::new();
        write_raw_frame(&mut buf, &frame).unwrap();
        let mut cursor = io::Cursor::new(buf);
        let back = read_raw_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(back.channel, "raw-pty-output");
        assert_eq!(back.kind, FrameKind::Raw);
        assert_eq!(back.data, vec![0x01, 0x02, 0x03, 0xff, 0x00]);
        // A second read hits clean EOF.
        assert!(read_raw_frame(&mut cursor).unwrap().is_none());
    }
}
