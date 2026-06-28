//! framelite-demo — an interactive particle simulation on the full framework spine.
//!
//! ```text
//! Clock ─tick─▶ Stepper ─┐
//!                        ├─actions─▶ World (Doc<SimState,SimAction>) ─state─▶ Renderer ─frame─▶ window
//! input (kbd/mouse) ─────┘                                                   (media data plane)
//! ```
//!
//! Every framework layer is exercised at once and under a real 60fps load:
//! * **time** — a `Clock` source ticks the simulation,
//! * **sources** — `Stepper` turns ticks into `Step` actions; an `InputMapper` turns keys and
//!   clicks into spawn/reset/gravity actions,
//! * **world** — a `Doc`-backed `WorldModule` reduces them and republishes state,
//! * **media** — the `Renderer` sink rasterizes state into frames sent over the zero-copy data
//!   plane (the pixels never touch the JSON bus),
//! * **observability** — the overlay polls `bus.metrics()` and `app.live_count()` live, so the
//!   framework's real cost is on screen. Spam-spawn and watch the `state` channel start to drop.
//!
//! Controls: **click** spawn at cursor · **Space** spawn at centre · **G** gravity · **R** reset.

mod render;
mod sim;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui;
use framelite_app::App;
use framelite_bus::{LocalBus, Receiver};
use framelite_core::{Channel, Module, ModuleCtx, ModuleId, Topic};
use framelite_document::Doc;
use framelite_input::{InputEvent, InputMapper, Key, MouseButton};
use framelite_media::{Frame, LatestReceiver};
use framelite_time::{Clock, Tick};

use render::Renderer;
use sim::{SimAction, SimState, HEIGHT, WIDTH};

/// Channel names — the wiring vocabulary shared by the modules.
const TICK: &str = "tick";
const ACTIONS: &str = "actions";
const STATE: &str = "state";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new();

    // The world: one Doc reducing every action into SimState, republishing it on `state`.
    app.world("world", ACTIONS, STATE, Doc::new(sim::initial(), sim::reduce));

    // Sources: a 60 Hz clock, and a Stepper translating each tick into a Step action.
    let actions: Topic<SimAction> = Topic::new(ACTIONS);
    app.source(Clock::hz("clock", TICK, 60.0));
    app.source(Stepper::new(actions.clone()));

    // Sink: the renderer reads state and emits frames on the media data plane.
    let (frame_tx, frame_rx) = framelite_media::latest::<Frame>();
    app.sink(Renderer::new(frame_tx));

    // Input is fed from the window (main thread): keys/clicks → SimAction on `actions`.
    let bus = app.bus();
    let mouse = Arc::new(Mutex::new((WIDTH as f32 / 2.0, HEIGHT as f32 / 2.0)));
    let mapper = InputMapper::new(bus.clone(), "input", actions, {
        let mouse = mouse.clone();
        move |ev| match ev {
            // A move updates the shared cursor (in sim space) and emits nothing…
            InputEvent::MouseMoved { x, y } => {
                *mouse.lock().unwrap() = (*x as f32, *y as f32);
                None
            }
            // …a left click spawns a burst there.
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            } => {
                let (x, y) = *mouse.lock().unwrap();
                Some(SimAction::Spawn { x, y, n: 60 })
            }
            InputEvent::KeyDown(Key::Space) => Some(SimAction::Spawn {
                x: WIDTH as f32 / 2.0,
                y: HEIGHT as f32 / 2.0,
                n: 80,
            }),
            InputEvent::KeyDown(Key::Char('g')) => Some(SimAction::ToggleGravity),
            InputEvent::KeyDown(Key::Char('r')) => Some(SimAction::Reset),
            _ => None,
        }
    });

    // A second consumer of `state` (besides the renderer): the overlay reads it for counts.
    let state_rx = bus.subscribe(STATE);

    let demo = DemoApp {
        app: Some(app),
        bus,
        frames: frame_rx,
        mapper,
        state_rx,
        tex: None,
        particles: 0,
        gravity: true,
        fps: 0.0,
        last: Instant::now(),
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([900.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "framelite demo — particles",
        native_options,
        Box::new(|_cc| Ok(Box::new(demo))),
    )
    .map_err(|e| e.to_string().into())
}

/// A source module: each clock `Tick` becomes a `Step` action for the world. Keeping this its
/// own module means the world never knows about the clock — it just reduces actions.
struct Stepper {
    id: ModuleId,
    actions: Topic<SimAction>,
}

impl Stepper {
    fn new(actions: Topic<SimAction>) -> Self {
        Self {
            id: ModuleId::new("stepper"),
            actions,
        }
    }
}

impl Module for Stepper {
    fn id(&self) -> ModuleId {
        self.id.clone()
    }

    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new(TICK)]
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let this = *self;
        while !ctx.should_stop() {
            match ctx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(Some(env)) => {
                    if let Ok(tick) = env.decode::<Tick>() {
                        let _ = ctx.publish_on(&this.actions, &SimAction::Step { dt: tick.dt as f32 });
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    }
}

/// The window: pulls frames off the data plane, displays them, feeds input, draws the overlay.
/// It is *not* a module — it is "the rest of the app", talking to the runtime only via the bus
/// and the media channel. It owns the `App` so it can shut the runtime down on close.
struct DemoApp {
    app: Option<App>,
    bus: Arc<LocalBus>,
    frames: LatestReceiver<Frame>,
    mapper: InputMapper<SimAction>,
    state_rx: Box<dyn Receiver>,
    tex: Option<egui::TextureHandle>,
    particles: usize,
    gravity: bool,
    fps: f32,
    last: Instant,
}

impl eframe::App for DemoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // --- fps (exponential smoothing) ---
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32();
        self.last = now;
        if dt > 0.0 {
            let inst = 1.0 / dt;
            self.fps = if self.fps == 0.0 { inst } else { self.fps * 0.9 + inst * 0.1 };
        }

        // --- pull the freshest rendered frame off the data plane and upload it ---
        let mut newest = None;
        while let Ok(Some(f)) = self.frames.try_recv() {
            newest = Some(f);
        }
        if let Some(frame) = newest {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [WIDTH as usize, HEIGHT as usize],
                &frame.pixels,
            );
            match &mut self.tex {
                Some(tex) => tex.set(image, egui::TextureOptions::NEAREST),
                None => {
                    self.tex =
                        Some(ctx.load_texture("frame", image, egui::TextureOptions::NEAREST))
                }
            }
        }

        // --- drain the world state (overlay reads count/gravity) ---
        // Drain to the newest envelope first, then decode *once* — decoding every queued state
        // would do many full deserializes per frame under load.
        let mut newest_state = None;
        while let Ok(Some(env)) = self.state_rx.try_recv() {
            newest_state = Some(env);
        }
        if let Some(s) = newest_state.and_then(|env| env.decode::<SimState>().ok()) {
            self.particles = s.particles.len();
            self.gravity = s.gravity;
        }

        // --- keyboard input → actions ---
        let (g, r, space) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::G),
                i.key_pressed(egui::Key::R),
                i.key_pressed(egui::Key::Space),
            )
        });
        if g {
            let _ = self.mapper.feed(&InputEvent::KeyDown(Key::Char('g')));
        }
        if r {
            let _ = self.mapper.feed(&InputEvent::KeyDown(Key::Char('r')));
        }
        if space {
            let _ = self.mapper.feed(&InputEvent::KeyDown(Key::Space));
        }

        // --- the canvas + mouse input ---
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(tex) = &self.tex {
                let avail = ui.available_size();
                let resp = ui.add(
                    egui::Image::new(&*tex)
                        .fit_to_exact_size(avail)
                        .sense(egui::Sense::click()),
                );
                // Map the cursor from the image rect into sim space, then feed it.
                if let Some(pos) = resp.hover_pos() {
                    let rect = resp.rect;
                    if rect.width() > 0.0 && rect.height() > 0.0 {
                        let nx = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                        let ny = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
                        let _ = self.mapper.feed(&InputEvent::MouseMoved {
                            x: (nx * WIDTH as f32) as f64,
                            y: (ny * HEIGHT as f32) as f64,
                        });
                    }
                }
                if resp.clicked() {
                    let _ = self.mapper.feed(&InputEvent::MouseButton {
                        button: MouseButton::Left,
                        pressed: true,
                    });
                }
            } else {
                ui.centered_and_justified(|ui| ui.label("starting…"));
            }
        });

        self.overlay(ctx);

        // Drive the loop continuously — this is a real-time animation, not an idle UI.
        ctx.request_repaint();
    }
}

impl DemoApp {
    /// The observability overlay: live bus metrics + module liveness, so the framework's cost
    /// is visible. This is where spamming particles shows up as drops on the `state` channel.
    fn overlay(&self, ctx: &egui::Context) {
        let metrics = self.bus.metrics();
        let live = self.app.as_ref().map(|a| a.live_count()).unwrap_or(0);

        egui::Window::new("framelite")
            .anchor(egui::Align2::LEFT_TOP, [8.0, 8.0])
            .resizable(false)
            .collapsible(true)
            .show(ctx, |ui| {
                ui.label(format!("fps        {:>6.1}", self.fps));
                ui.label(format!("particles  {:>6}", self.particles));
                ui.label(format!(
                    "gravity    {:>6}",
                    if self.gravity { "on" } else { "off" }
                ));
                ui.label(format!("modules    {:>6}", live));
                ui.separator();
                ui.label("channel    pub/s? published  dropped  subs");
                for ch in [TICK, ACTIONS, STATE] {
                    let m = metrics
                        .channels
                        .get(&Channel::new(ch))
                        .cloned()
                        .unwrap_or_default();
                    ui.label(format!(
                        "{:<10} {:>9}  {:>7}  {:>4}",
                        ch, m.published, m.dropped, m.subscribers
                    ));
                }
                ui.separator();
                ui.label(format!("total dropped  {}", metrics.total_dropped));
                ui.weak("click: spawn · space: burst · G: gravity · R: reset");
                ui.weak("pixels ride the media data plane, not the bus");
            });
    }
}

impl Drop for DemoApp {
    fn drop(&mut self) {
        // Window closed → stop the runtime and join its modules cleanly.
        if let Some(app) = self.app.take() {
            let report = app.shutdown_and_join();
            if !report.is_clean() {
                eprintln!("modules panicked: {:?}", report.panicked);
            }
        }
    }
}
