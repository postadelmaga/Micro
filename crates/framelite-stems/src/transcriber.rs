//! Stage 2 — **transcription** (basic-pitch). A transform module: it listens for stems and,
//! for each melodic one, runs Spotify's basic-pitch to produce a `.mid`. Each stem is handed
//! to the runtime's **worker pool** via [`ModuleCtx::offload`], so the stems transcribe in
//! parallel instead of one-by-one on this module's thread.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use framelite_bus::LocalBus;
use framelite_core::{Channel, Envelope, Module, ModuleCtx, ModuleId};

use crate::messages::{Progress, Stem, StemMidi, StemsReady, MIDI, PROGRESS, STEMS};
use crate::tools;

pub struct Transcriber {
    /// Directory the `.mid` files are written to.
    out: PathBuf,
}

impl Transcriber {
    pub fn new(out: PathBuf) -> Self {
        Self { out }
    }
}

impl Module for Transcriber {
    fn id(&self) -> ModuleId {
        ModuleId::new("transcriber")
    }

    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new(STEMS)]
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let this = *self;
        while !ctx.should_stop() {
            match ctx.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(env)) => {
                    if let Ok(ready) = env.decode::<StemsReady>() {
                        dispatch(&this, &ctx, ready);
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    }
}

/// Fan the stems out onto the worker pool: one basic-pitch run each, in parallel.
fn dispatch(this: &Transcriber, ctx: &ModuleCtx, ready: StemsReady) {
    for stem in ready.stems {
        if !stem.melodic {
            publish_progress(
                &ctx.bus(),
                format!("skipping {} (percussion — not pitched)", stem.name),
            );
            continue;
        }
        let bus = ctx.bus();
        let out = this.out.clone();
        let track = ready.track.clone();
        // Runs on a pool thread; publishes its result back on the bus when done.
        ctx.offload(move || {
            let outcome = transcribe(&stem, &out, &track);
            let _ = bus.publish(
                Envelope::encode(ModuleId::new("transcriber"), MIDI, &outcome)
                    .unwrap_or_else(|e| Envelope::new(ModuleId::new("transcriber"), MIDI, e.into())),
            );
        });
    }
}

/// Run basic-pitch on one stem and return where its MIDI landed (renamed to `<track>__<stem>.mid`).
fn transcribe(stem: &Stem, out: &Path, track: &str) -> StemMidi {
    // basic-pitch OUTPUT_DIR INPUT_AUDIO  → writes <input_stem>_basic_pitch.mid in OUTPUT_DIR.
    let result = Command::new(tools::basic_pitch_bin())
        .arg(out)
        .arg(&stem.wav)
        .output();

    match result {
        Ok(o) if o.status.success() => {
            let produced = produced_path(out, &stem.wav);
            let tidy = out.join(format!("{track}__{}.mid", stem.name));
            if produced.exists() {
                let _ = std::fs::rename(&produced, &tidy);
                StemMidi::Ok {
                    name: stem.name.clone(),
                    midi: tidy.to_string_lossy().into_owned(),
                }
            } else {
                StemMidi::Failed {
                    name: stem.name.clone(),
                    error: format!("basic-pitch produced no MIDI at {}", produced.display()),
                }
            }
        }
        Ok(o) => StemMidi::Failed {
            name: stem.name.clone(),
            error: format!(
                "basic-pitch exited with {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("")
            ),
        },
        Err(e) => StemMidi::Failed {
            name: stem.name.clone(),
            error: format!("could not run basic-pitch: {e}"),
        },
    }
}

/// basic-pitch names its output `<input_filestem>_basic_pitch.mid` in the output dir.
fn produced_path(out: &Path, wav: &str) -> PathBuf {
    let base = Path::new(wav)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("stem");
    out.join(format!("{base}_basic_pitch.mid"))
}

fn publish_progress(bus: &LocalBus, msg: String) {
    let _ = bus.publish(
        Envelope::encode(
            ModuleId::new("transcriber"),
            PROGRESS,
            &Progress {
                stage: "transcribe".into(),
                msg,
            },
        )
        .expect("progress is serializable"),
    );
}
