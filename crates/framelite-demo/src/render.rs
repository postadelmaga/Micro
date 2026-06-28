//! The **render sink**: a module that turns world state into pixels.
//!
//! It subscribes to the world's `state` channel (control plane, JSON), rasterizes the latest
//! `SimState` into an RGBA [`Frame`], and pushes it on a [`framelite_media::latest`] channel
//! (data plane, zero-copy) toward the window. The split is the whole point: the *description*
//! of the world rides the bus; the *megabytes of pixels* never touch it.

use framelite_core::{Channel, Module, ModuleCtx, ModuleId};
use framelite_media::{Frame, LatestSender, PixelFormat};

use crate::sim::{SimState, HEIGHT, WIDTH};
use crate::STATE;

const BG: [u8; 4] = [16, 18, 24, 255];
const RADIUS: i32 = 4;

/// A sink module: world `state` in (bus), `Frame`s out (media data plane).
pub struct Renderer {
    id: ModuleId,
    frames: LatestSender<Frame>,
}

impl Renderer {
    pub fn new(frames: LatestSender<Frame>) -> Self {
        Self {
            id: ModuleId::new("renderer"),
            frames,
        }
    }
}

impl Module for Renderer {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new(STATE)]
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let this = *self;
        let mut buf = vec![0u8; (WIDTH * HEIGHT * 4) as usize];

        while !ctx.should_stop() {
            match ctx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(Some(first)) => {
                    // The world may have published several states since we last drew; only the
                    // newest matters, so drain to it (latest-wins on the control side too).
                    let mut latest = first;
                    while let Ok(Some(env)) = ctx.try_recv() {
                        latest = env;
                    }
                    if let Ok(state) = latest.decode::<SimState>() {
                        rasterize(&state, &mut buf);
                        // One copy into a shared Arc buffer; from here the frame moves by
                        // pointer. If the window hasn't taken the previous frame, it's dropped
                        // (latest-wins) — correct for video.
                        if let Ok(frame) =
                            Frame::new(WIDTH, HEIGHT, PixelFormat::Rgba8, buf.clone())
                        {
                            let _ = this.frames.send(frame);
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => break, // bus closed
            }
        }
    }
}

/// Clear to the background and stamp a filled disc for every particle.
fn rasterize(state: &SimState, buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        px.copy_from_slice(&BG);
    }
    for p in &state.particles {
        draw_disc(buf, p.x as i32, p.y as i32, RADIUS, hue_rgb(p.hue));
    }
}

fn draw_disc(buf: &mut [u8], cx: i32, cy: i32, r: i32, rgb: [u8; 3]) {
    let (w, h) = (WIDTH as i32, HEIGHT as i32);
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r * r {
                continue;
            }
            let (x, y) = (cx + dx, cy + dy);
            if x < 0 || x >= w || y < 0 || y >= h {
                continue;
            }
            let i = ((y * w + x) * 4) as usize;
            buf[i] = rgb[0];
            buf[i + 1] = rgb[1];
            buf[i + 2] = rgb[2];
            buf[i + 3] = 255;
        }
    }
}

/// Full-saturation HSV→RGB for a hue in `0.0..1.0` — enough for lively colours, no deps.
fn hue_rgb(hue: f32) -> [u8; 3] {
    let h = hue.fract().abs() * 6.0;
    let f = h - h.floor();
    let (r, g, b) = match h as i32 % 6 {
        0 => (1.0, f, 0.0),
        1 => (1.0 - f, 1.0, 0.0),
        2 => (0.0, 1.0, f),
        3 => (0.0, 1.0 - f, 1.0),
        4 => (f, 0.0, 1.0),
        _ => (1.0, 0.0, 1.0 - f),
    };
    [(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8]
}
