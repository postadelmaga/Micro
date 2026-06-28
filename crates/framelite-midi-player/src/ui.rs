//! The window: an eframe/egui app on the main thread. It is *not* a framelite module — it is
//! "the rest of the app". It publishes [`TransportCmd`]s on the bus and renders the latest
//! [`Status`] it receives. It knows nothing about cpal or rustysynth.

use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use framelite_bus::{LocalBus, Receiver};
use framelite_protocol::{Envelope, ModuleId};

use crate::messages::{Status, TransportCmd, STATUS, TRANSPORT};

pub struct PlayerApp {
    bus: Arc<LocalBus>,
    status_rx: Box<dyn Receiver>,
    status: Status,
    /// Local slider state, mirrored to the engine on change.
    volume: f32,
    reverb: f32,
}

impl PlayerApp {
    pub fn new(bus: Arc<LocalBus>) -> Self {
        let status_rx = bus.subscribe(STATUS);
        Self {
            bus,
            status_rx,
            status: Status::default(),
            volume: 1.0,
            reverb: 0.15,
        }
    }

    fn send(&self, cmd: TransportCmd) {
        if let Ok(env) = Envelope::encode(ModuleId::new("ui"), TRANSPORT, &cmd) {
            let _ = self.bus.publish(env);
        }
    }
}

impl eframe::App for PlayerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Pull every status update the engine has published since the last frame.
        while let Ok(Some(env)) = self.status_rx.try_recv() {
            if let Ok(s) = env.decode::<Status>() {
                self.status = s;
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("◇ framelite — MIDI player");
            ui.separator();

            ui.horizontal(|ui| {
                if ui.button("🎹 SoundFont (.sf2)…").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("SoundFont", &["sf2"])
                        .pick_file()
                    {
                        self.send(TransportCmd::LoadSoundFont(p.to_string_lossy().into_owned()));
                    }
                }
                if ui.button("📂 MIDI (.mid)…").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("MIDI", &["mid", "midi"])
                        .pick_file()
                    {
                        self.send(TransportCmd::LoadMidi(p.to_string_lossy().into_owned()));
                    }
                }
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(format!("SoundFont: {}", dash(&self.status.soundfont)));
                if self.status.soundfont.is_none() && ui.button("⬇ get one (32 MB)").clicked() {
                    self.send(TransportCmd::DownloadSoundFont);
                }
            });
            ui.label(format!("MIDI: {}", dash(&self.status.midi)));
            ui.separator();

            ui.horizontal(|ui| {
                if ui.button("▶  Play").clicked() {
                    self.send(TransportCmd::Play);
                }
                if ui.button("■  Stop").clicked() {
                    self.send(TransportCmd::Stop);
                }
                ui.label(if self.status.playing {
                    "playing"
                } else {
                    "stopped"
                });
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Volume");
                if ui
                    .add(egui::Slider::new(&mut self.volume, 0.0..=1.0))
                    .changed()
                {
                    self.send(TransportCmd::SetVolume(self.volume));
                }
            });
            ui.horizontal(|ui| {
                ui.label("Reverb");
                if ui
                    .add(egui::Slider::new(&mut self.reverb, 0.0..=0.6))
                    .changed()
                {
                    self.send(TransportCmd::SetReverb(self.reverb));
                }
            });

            ui.add_space(8.0);
            let (pos, dur) = (self.status.position, self.status.duration);
            let frac = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) } else { 0.0 };
            ui.add(
                egui::ProgressBar::new(frac)
                    .text(format!("{} / {}", fmt_time(pos), fmt_time(dur))),
            );

            if let Some(msg) = &self.status.message {
                ui.add_space(4.0);
                ui.weak(msg);
            }

            ui.add_space(8.0);
            ui.collapsing("Per-channel routing (auto from GM instrument)", |ui| {
                for line in self.status.routing.lines() {
                    ui.weak(line);
                }
            });
        });

        // Keep the progress bar/clock live while playing.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn dash(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "—".into())
}

fn fmt_time(secs: f32) -> String {
    let s = secs.max(0.0) as u32;
    format!("{}:{:02}", s / 60, s % 60)
}
