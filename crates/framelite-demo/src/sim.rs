//! The **world**: simulation state, the actions that change it, and the pure reducer.
//!
//! This is exactly the shape [`framelite_document::Doc`] wants — `SimState` is the truth,
//! every change is a serializable `SimAction`, and `reduce` is a pure function. The physics
//! step is an action too (`Step`), so the clock, the keyboard, and the mouse all feed the
//! world through one uniform channel and the world stays a plain reducer.

use serde::{Deserialize, Serialize};

/// Render/simulation canvas size, in pixels (the sim works in this fixed space; the window
/// scales the result to fit).
pub const WIDTH: u32 = 800;
pub const HEIGHT: u32 = 600;
const W: f32 = WIDTH as f32;
const H: f32 = HEIGHT as f32;

/// Above this we stop spawning — a guard so "spam spawn" stresses the *pipeline* (serialize/
/// fan-out/rasterize) rather than just running out of memory.
const MAX_PARTICLES: usize = 8000;

/// Gravity acceleration (px/s²) when enabled.
const GRAVITY: f32 = 700.0;
/// Velocity kept after a wall bounce.
const RESTITUTION: f32 = 0.88;

/// One particle. Plain data so the whole world serializes across the bus each tick.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Particle {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    /// Colour hue in `0.0..1.0`, set at spawn.
    pub hue: f32,
}

/// The entire simulation state — the single source of truth held by the world's `Doc`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SimState {
    pub particles: Vec<Particle>,
    pub gravity: bool,
    /// Simulated seconds elapsed.
    pub t: f64,
    /// RNG state, advanced *inside* the reducer so spawns stay deterministic — replaying the
    /// same action log reproduces the same world, honouring the `Doc` contract.
    seed: u64,
}

impl Default for SimState {
    fn default() -> Self {
        Self {
            particles: Vec::new(),
            gravity: true,
            t: 0.0,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// Every way the world can change. `Step` is the physics tick (driven by the clock); the rest
/// come from input. All ride the one `actions` channel into the world.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum SimAction {
    /// Advance the simulation by `dt` seconds.
    Step { dt: f32 },
    /// Spawn `n` particles bursting from `(x, y)`.
    Spawn { x: f32, y: f32, n: u32 },
    ToggleGravity,
    Reset,
}

/// The pure reducer handed to `Doc::new`. Deterministic given the state and the action.
pub fn reduce(s: &mut SimState, a: &SimAction) -> Result<(), String> {
    match a {
        SimAction::Step { dt } => step(s, *dt),
        SimAction::Spawn { x, y, n } => spawn(s, *x, *y, *n),
        SimAction::ToggleGravity => s.gravity = !s.gravity,
        SimAction::Reset => {
            s.particles.clear();
            s.t = 0.0;
        }
    }
    Ok(())
}

/// A world seeded with a starter burst, so the window shows motion immediately.
pub fn initial() -> SimState {
    let mut s = SimState::default();
    spawn(&mut s, W / 2.0, H / 2.0, 200);
    s
}

fn step(s: &mut SimState, dt: f32) {
    // Clamp dt so a scheduling hiccup can't teleport everything through the walls.
    let dt = dt.min(1.0 / 30.0);
    let g = if s.gravity { GRAVITY } else { 0.0 };
    for p in &mut s.particles {
        p.vy += g * dt;
        p.x += p.vx * dt;
        p.y += p.vy * dt;
        if p.x < 0.0 {
            p.x = 0.0;
            p.vx = -p.vx * RESTITUTION;
        } else if p.x > W {
            p.x = W;
            p.vx = -p.vx * RESTITUTION;
        }
        if p.y < 0.0 {
            p.y = 0.0;
            p.vy = -p.vy * RESTITUTION;
        } else if p.y > H {
            p.y = H;
            p.vy = -p.vy * RESTITUTION;
        }
    }
    s.t += dt as f64;
}

fn spawn(s: &mut SimState, x: f32, y: f32, n: u32) {
    for _ in 0..n {
        if s.particles.len() >= MAX_PARTICLES {
            break;
        }
        let angle = rand01(&mut s.seed) * std::f32::consts::TAU;
        let speed = 80.0 + rand01(&mut s.seed) * 240.0;
        let hue = rand01(&mut s.seed);
        s.particles.push(Particle {
            x,
            y,
            vx: angle.cos() * speed,
            // Bias the initial burst slightly upward so gravity has something to pull back.
            vy: angle.sin() * speed - 60.0,
            hue,
        });
    }
}

/// xorshift64 → `[0.0, 1.0)`. Tiny, deterministic, state lives in `SimState::seed`.
fn rand01(seed: &mut u64) -> f32 {
    let mut x = *seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *seed = x;
    // Top 24 bits → a float in [0,1).
    ((x >> 40) as f32) / (1u32 << 24) as f32
}
