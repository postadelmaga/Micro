//! framelite MIDI player — a desktop app on top of framelite.
//!
//! `cargo run -p framelite-midi-player`
//!
//! Wiring, in the `sources → world → sinks` shape: the window is a *source* (it publishes
//! `transport` commands) that also renders; the [`AudioEngine`](audio::AudioEngine) is a
//! *sink* (it consumes `transport` and republishes `status`). There is no pure `world` node
//! here — the engine's authoritative state lives in a cpal stream and a synth, which a
//! reducer can't own — so the app is honestly two ends wired by the bus, composed with
//! [`App`](framelite_app::App). The two never call each other; only the bus connects them.

mod audio;
mod effects;
mod messages;
mod reverb;
mod router;
mod ui;

use framelite_app::App;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new();
    // `status` is durable state: a window that subscribes after the engine starts still gets
    // the latest snapshot replayed to it.
    app.retain(messages::STATUS);
    // The audio engine consumes `transport` and produces `status` — a sink.
    app.sink(audio::AudioEngine);

    let window = ui::PlayerApp::new(app.bus());
    let native_options = eframe::NativeOptions {
        viewport: egui_viewport(),
        ..Default::default()
    };

    // Runs the window on this (main) thread until it is closed.
    let result = eframe::run_native(
        "framelite midi player",
        native_options,
        Box::new(|_cc| Ok(Box::new(window))),
    );

    // Window closed → wind the modules down and report any panic.
    let report = app.shutdown_and_join();
    if !report.is_clean() {
        eprintln!("modules panicked: {:?}", report.panicked);
    }

    result.map_err(|e| e.to_string().into())
}

fn egui_viewport() -> eframe::egui::ViewportBuilder {
    eframe::egui::ViewportBuilder::default().with_inner_size([440.0, 300.0])
}
