//! # micro-video — the video sink adapter
//!
//! A video sink *pulls* frames from the [media data plane](micro_media) — the zero-copy,
//! `Arc`-backed side — and puts them on screen. It deliberately ignores the JSON bus: a 1080p
//! RGBA frame is ~8 MB, and serializing 60 of those a second is a non-starter. The bus only
//! ever carries the tiny "frame ready" control message; the bytes travel here, by ownership.
//!
//! The actual GPU/window backend (wgpu, a swapchain, a window) is the *app's* job — that is
//! the platform weight micro stays out of. This crate is just the framework-side contract:
//! the [`FrameSink`] trait an app implements to present a frame, the [`VideoSink`] module that
//! drives it off the data plane, and a headless [`BufferSink`] so the whole path is testable
//! without a GPU.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use micro_core::{Channel, Module, ModuleCtx, ModuleId};
use micro_media::{Frame, LatestReceiver};

/// Implemented by an app to put a frame on screen. The real impl uploads `frame.pixels` to a
/// GPU texture and presents; the framework only needs this one call. `&mut self` so a backend
/// can hold mutable state (a swapchain, a reusable staging buffer) across presents.
pub trait FrameSink: Send {
    fn present(&mut self, frame: &Frame);
}

/// A module that pumps the freshest [`Frame`] from a [`latest`](micro_media::latest) mailbox
/// into a [`FrameSink`]. It owns its sink and reads only the data plane — no bus subscriptions.
pub struct VideoSink<S: FrameSink> {
    id: ModuleId,
    frames: LatestReceiver<Frame>,
    sink: S,
}

impl<S: FrameSink> VideoSink<S> {
    pub fn new(id: impl Into<String>, frames: LatestReceiver<Frame>, sink: S) -> Self {
        Self {
            id: ModuleId::new(id),
            frames,
            sink,
        }
    }
}

impl<S: FrameSink + 'static> Module for VideoSink<S> {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    // No bus subscriptions: a video sink lives entirely on the data plane.
    fn subscriptions(&self) -> Vec<Channel> {
        Vec::new()
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        // `run` hands us an owned box; move out so we can mutate `sink` in the loop.
        let mut this = *self;
        while !ctx.should_stop() {
            match this.frames.try_recv() {
                // Newest frame wins — present it. Stale frames were already coalesced away.
                Ok(Some(frame)) => this.sink.present(&frame),
                // Nothing ready: poll at ~250 Hz so we stay responsive to `should_stop`
                // without busy-spinning a core. (No blocking `recv` — it would ignore stop.)
                Ok(None) => std::thread::sleep(Duration::from_millis(4)),
                // Producer is gone for good; there will never be another frame.
                Err(()) => break,
            }
        }
    }
}

// --- headless reference sink (tests, debug overlays) ---------------------------------------

/// What a [`BufferSink`] records: the most recent frame and how many it has presented. Shared
/// behind an `Arc<Mutex<_>>` so a test (or a debug HUD) can read it while the sink runs.
pub struct BufferState {
    pub last: Option<Frame>,
    pub count: u64,
}

/// A `FrameSink` that presents nowhere — it just remembers the last frame and a count. Lets the
/// whole video path be exercised without a GPU.
pub struct BufferSink {
    state: Arc<Mutex<BufferState>>,
}

impl BufferSink {
    /// Returns the sink plus a handle onto its shared state, so the caller can inspect what the
    /// sink has seen.
    pub fn new() -> (BufferSink, Arc<Mutex<BufferState>>) {
        let state = Arc::new(Mutex::new(BufferState {
            last: None,
            count: 0,
        }));
        (
            BufferSink {
                state: Arc::clone(&state),
            },
            state,
        )
    }
}

impl FrameSink for BufferSink {
    fn present(&mut self, frame: &Frame) {
        // Frame clone is a pointer move (pixels are `Arc`-backed), so recording is cheap.
        let mut state = self.state.lock().unwrap();
        state.last = Some(frame.clone());
        state.count += 1;
    }
}
