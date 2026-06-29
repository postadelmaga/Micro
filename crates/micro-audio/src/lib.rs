//! # micro-audio — the audio sink adapter
//!
//! The audio sink pulls [`AudioBlock`]s from the **data plane** ([`micro_media`]), not the
//! serializing JSON bus: audio is high-bandwidth and must never be encoded. It rides a
//! [`bounded`](micro_media::bounded), **lossless** channel, so a slow consumer applies
//! backpressure to the producer instead of dropping samples — for audio, pacing is correct and
//! dropping is an audible glitch.
//!
//! The sink is generic over an [`AudioOut`] device contract. The default build ships only the
//! headless [`Recorder`] (used in tests), so it compiles and runs without any audio hardware.
//! A real device backend, [`CpalOut`], lives behind the OFF-by-default `cpal` feature.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use micro_core::{Channel, Module, ModuleCtx, ModuleId};
use micro_media::{AudioBlock, BoundedReceiver};

/// A device that can play an [`AudioBlock`]. Implement this to drive real hardware; the sink
/// hands it blocks one at a time, in order, off the data plane.
pub trait AudioOut: Send {
    /// Play one block. The slice is borrowed — copy out anything you need to keep.
    fn play(&mut self, block: &AudioBlock);
}

/// A [`Module`] that drains [`AudioBlock`]s from a bounded data-plane channel into an
/// [`AudioOut`] device. It owns the receiver, so backpressure reaches the producer directly.
pub struct AudioSink<O: AudioOut> {
    id: ModuleId,
    blocks: BoundedReceiver<AudioBlock>,
    out: O,
}

impl<O: AudioOut> AudioSink<O> {
    /// Build a sink reading from `blocks` and playing into `out`.
    pub fn new(id: impl Into<String>, blocks: BoundedReceiver<AudioBlock>, out: O) -> Self {
        Self {
            id: ModuleId::new(id),
            blocks,
            out,
        }
    }
}

impl<O: AudioOut + 'static> Module for AudioSink<O> {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    // Nothing on the control bus: blocks arrive on the data plane.
    fn subscriptions(&self) -> Vec<Channel> {
        Vec::new()
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        // `run` owns `self`; move out of the box so we can mutate the device in the loop.
        let mut this = *self;
        while !ctx.should_stop() {
            // Time out periodically so a quiet stream still observes the shutdown signal.
            match this.blocks.recv_timeout(Duration::from_millis(50)) {
                Ok(block) => this.out.play(&block),
                Err(RecvTimeoutError::Timeout) => {} // re-check should_stop
                Err(RecvTimeoutError::Disconnected) => break, // producer gone, nothing more to play
            }
        }
    }
}

/// A headless [`AudioOut`] that captures every sample played into a shared buffer. The
/// reference output for tests and debugging — no device, fully deterministic.
pub struct Recorder {
    samples: Arc<Mutex<Vec<f32>>>,
}

impl Recorder {
    /// Build a recorder and a handle to the buffer it appends to (the caller inspects it).
    pub fn new() -> (Recorder, Arc<Mutex<Vec<f32>>>) {
        let samples = Arc::new(Mutex::new(Vec::new()));
        (
            Recorder {
                samples: samples.clone(),
            },
            samples,
        )
    }
}

impl AudioOut for Recorder {
    fn play(&mut self, block: &AudioBlock) {
        self.samples.lock().unwrap().extend_from_slice(&block.samples);
    }
}

// --- optional real device backend ----------------------------------------------------------

/// A cpal-backed [`AudioOut`] that plays blocks through the default output device.
///
/// `play` appends interleaved samples into a shared ring; the cpal callback (on the audio
/// thread) drains the ring, writing silence when it runs dry. Behind the `cpal` feature so the
/// default build never pulls a device dependency.
#[cfg(feature = "cpal")]
pub struct CpalOut {
    ring: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<f32>>>,
    // The stream stops when dropped, so keep it alive for the device's lifetime.
    _stream: SendStream,
}

/// cpal marks `Stream` `!Send` on every platform, but [`AudioSink`] moves its [`AudioOut`] onto
/// its own thread (so [`AudioOut`] is `Send`). We only ever *hold then drop* the stream — never
/// touch it across threads in a racy way — so asserting `Send` here is sound for our use.
#[cfg(feature = "cpal")]
struct SendStream(#[allow(dead_code)] cpal::Stream); // held only to keep the stream playing
#[cfg(feature = "cpal")]
unsafe impl Send for SendStream {}

#[cfg(feature = "cpal")]
impl CpalOut {
    /// Open the default output device and start a stream that drains this sink's ring.
    pub fn new() -> Result<CpalOut, String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no output audio device")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let sample_format = config.sample_format();
        let config: cpal::StreamConfig = config.into();

        let ring = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));

        let stream = match sample_format {
            cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, &ring)?,
            cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, &ring)?,
            cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config, &ring)?,
            other => return Err(format!("unsupported sample format: {other:?}")),
        };
        stream.play().map_err(|e| e.to_string())?;

        Ok(CpalOut {
            ring,
            _stream: SendStream(stream),
        })
    }
}

#[cfg(feature = "cpal")]
impl AudioOut for CpalOut {
    fn play(&mut self, block: &AudioBlock) {
        // Hand the interleaved samples to the audio thread; the bounded channel upstream is
        // what bounds memory, so we simply enqueue.
        self.ring.lock().unwrap().extend(block.samples.iter().copied());
    }
}

/// Build an output stream for device sample type `T` whose callback pops from `ring`.
#[cfg(feature = "cpal")]
fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    ring: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<f32>>>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;

    let ring = ring.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let mut ring = ring.lock().unwrap();
                for sample in data.iter_mut() {
                    // Underrun → silence, rather than starving the device into an error.
                    let v = ring.pop_front().unwrap_or(0.0);
                    *sample = T::from_sample(v);
                }
            },
            |err| eprintln!("audio stream error: {err}"),
            None,
        )
        .map_err(|e| e.to_string())
}
