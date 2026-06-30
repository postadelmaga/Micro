//! # Cross-process & in-process transports over the bus traits
//!
//! Micro ships only the in-process [`LocalBus`](crate::LocalBus) broker in `lib.rs`; this
//! module adds the transports an app actually needs to host a module *out of process* — a
//! thread-channel pair, a stdio codec (JSON lines or length-prefixed postcard), and the wrappers
//! a supervisor uses to talk to a spawned child. Each is just an `impl` of Micro's identical
//! [`Sender`](crate::Sender) / [`Receiver`](crate::Receiver) traits, so module code written
//! against the traits is unaffected by which transport hosts it — "write once, host anywhere".
//!
//! Gated behind the `ipc` feature so a pure in-process app pulls none of the memmap/serde
//! transport weight.

use std::collections::VecDeque;
use std::io::{self, BufRead, Read, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::{BusError, Channel, Envelope, ModuleId, Receiver, Sender};

// --- IPC format selection ------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcFormat {
    Json,
    Postcard,
}

pub const IPC_FORMAT_ENV: &str = "MICRO_IPC_FORMAT";

pub fn ipc_format() -> IpcFormat {
    match std::env::var(IPC_FORMAT_ENV) {
        Ok(v) if v.trim().eq_ignore_ascii_case("postcard") => IpcFormat::Postcard,
        _ => IpcFormat::Json,
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PostcardEnvelope {
    from: ModuleId,
    channel: Channel,
    payload_json: String,
}

pub fn serialize_envelope_postcard(env: &Envelope) -> Result<Vec<u8>, BusError> {
    let payload_json = serde_json::to_string(&env.payload).map_err(|e| e.to_string())?;
    let pc = PostcardEnvelope {
        from: env.from.clone(),
        channel: env.channel.clone(),
        payload_json,
    };
    postcard::to_allocvec(&pc).map_err(|e| e.to_string())
}

pub fn deserialize_envelope_postcard(bytes: &[u8]) -> Result<Envelope, BusError> {
    let pc: PostcardEnvelope = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
    let payload = serde_json::from_str(&pc.payload_json).map_err(|e| e.to_string())?;
    Ok(Envelope {
        from: pc.from,
        channel: pc.channel,
        payload,
    })
}

// --- in-process transport (thread mpsc, zero serialization) --------------------

struct LocalSender(mpsc::Sender<Envelope>);

impl Sender for LocalSender {
    fn send(&self, env: Envelope) -> Result<(), BusError> {
        self.0.send(env).map_err(|e| e.to_string())
    }
}

struct LocalReceiver(mpsc::Receiver<Envelope>);

impl Receiver for LocalReceiver {
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

/// In-process pair: the default path, zero serialization, ~ns latency.
pub fn create_channel_pair() -> (Box<dyn Sender>, Box<dyn Receiver>) {
    let (tx, rx) = mpsc::channel();
    (Box::new(LocalSender(tx)), Box::new(LocalReceiver(rx)))
}

// --- cross-process transport (stdio JSON-lines or Postcard-binary) -------------

/// Writes envelopes as JSON lines or binary postcard packets to a stream.
/// Public so a supervisor can wrap a spawned child's stdin.
pub struct ProcessSender {
    writer: Arc<Mutex<dyn Write + Send>>,
}

impl ProcessSender {
    pub fn new<W: Write + Send + 'static>(writer: W) -> Self {
        Self { writer: Arc::new(Mutex::new(writer)) }
    }
}

impl Sender for ProcessSender {
    fn send(&self, env: Envelope) -> Result<(), BusError> {
        let mut w = self.writer.lock().map_err(|e| e.to_string())?;
        match ipc_format() {
            IpcFormat::Json => {
                let line = serde_json::to_string(&env).map_err(|e| e.to_string())?;
                writeln!(w, "{line}").map_err(|e| e.to_string())?;
            }
            IpcFormat::Postcard => {
                let bytes = serialize_envelope_postcard(&env)?;
                let len = bytes.len() as u32;
                w.write_all(&len.to_be_bytes()).map_err(|e| e.to_string())?;
                w.write_all(&bytes).map_err(|e| e.to_string())?;
            }
        }
        w.flush().map_err(|e| e.to_string())
    }
}

struct ProcessReceiver {
    reader: Mutex<Box<dyn BufRead + Send>>,
    buffer: Mutex<VecDeque<Envelope>>,
}

impl ProcessReceiver {
    /// Wrap any line-buffered reader as an envelope source. Used for our own stdin and,
    /// via [`receiver_from_reader`], for a child's piped stdout (the parent's return path).
    fn new(reader: impl BufRead + Send + 'static) -> Self {
        Self {
            reader: Mutex::new(Box::new(reader)),
            buffer: Mutex::new(VecDeque::new()),
        }
    }
}

impl Receiver for ProcessReceiver {
    fn recv(&self) -> Result<Envelope, BusError> {
        if let Some(env) = self.buffer.lock().unwrap().pop_front() {
            return Ok(env);
        }
        let mut reader = self.reader.lock().map_err(|e| e.to_string())?;
        match ipc_format() {
            IpcFormat::Json => {
                let mut line = String::new();
                let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err("EOF reached".into());
                }
                serde_json::from_str(&line).map_err(|e| e.to_string())
            }
            IpcFormat::Postcard => {
                let mut len_bytes = [0u8; 4];
                match reader.read_exact(&mut len_bytes) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        return Err("EOF reached".into());
                    }
                    Err(e) => return Err(e.to_string()),
                }
                let len = u32::from_be_bytes(len_bytes) as usize;
                let mut buf = vec![0u8; len];
                reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
                deserialize_envelope_postcard(&buf)
            }
        }
    }
    fn try_recv(&self) -> Result<Option<Envelope>, BusError> {
        // std stdin has no easy non-blocking mode, so process consumers run `recv()`
        // on a dedicated thread; `try_recv` only drains anything already buffered.
        Ok(self.buffer.lock().unwrap().pop_front())
    }
    fn recv_timeout(&self, _timeout: Duration) -> Result<Option<Envelope>, BusError> {
        // A piped/stdin reader has no portable timed read; its consumers drive it with a
        // dedicated `recv()` thread, so the timeout is best-effort: return anything already
        // buffered, otherwise block on the next line. Not used on the hot in-process path.
        if let Some(env) = self.buffer.lock().unwrap().pop_front() {
            return Ok(Some(env));
        }
        self.recv().map(Some)
    }
}

/// Cross-process pair over our own stdio: read envelopes from stdin, write to stdout.
/// A sidecar child uses this; the parent writes to the child's piped stdin via a
/// [`ProcessSender`].
pub fn create_stdio_pair() -> (Box<dyn Sender>, Box<dyn Receiver>) {
    let sender = ProcessSender::new(io::stdout());
    let receiver = ProcessReceiver::new(io::BufReader::new(io::stdin()));
    (Box::new(sender), Box::new(receiver))
}

/// Build a [`Receiver`] over an arbitrary line-buffered reader — the parent's **return
/// path**: wrap a spawned child's piped stdout so envelopes the sidecar emits flow back in
/// as JSON lines, one [`Envelope`] per `recv()`. Same wire format as [`create_stdio_pair`]
/// (which reads our own stdin); this just lets the caller supply the reader.
pub fn receiver_from_reader(r: impl BufRead + Send + 'static) -> Box<dyn Receiver> {
    Box::new(ProcessReceiver::new(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiver_from_reader_round_trips_json_lines() {
        // Two envelopes as JSON lines, exactly as a child's stdout would emit them.
        let env = Envelope::new(
            ModuleId::new("sidecar"),
            Channel::new("scene"),
            serde_json::json!({ "hello": "world" }),
        );
        let mut bytes = serde_json::to_vec(&env).unwrap();
        bytes.push(b'\n');
        bytes.extend_from_slice(&serde_json::to_vec(&env).unwrap());
        bytes.push(b'\n');

        let rx = receiver_from_reader(io::Cursor::new(bytes));

        let got = rx.recv().unwrap();
        assert_eq!(got.from, ModuleId::new("sidecar"));
        assert_eq!(got.channel, Channel::new("scene"));
        assert_eq!(got.payload, serde_json::json!({ "hello": "world" }));

        // Second line still decodes; then EOF surfaces as an error.
        assert!(rx.recv().is_ok());
        assert!(rx.recv().is_err());
    }
}
