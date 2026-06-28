//! The typed messages the pipeline stages exchange over the bus. Note what they carry: file
//! **paths** and **progress**, never audio bytes. The stems and MIDI live on disk; the bus
//! only coordinates the stages that read and write them.

use serde::{Deserialize, Serialize};

/// Separator → everyone: the stems Demucs produced.
pub const STEMS: &str = "stems";
/// Transcriber → reporter: one stem's transcription outcome.
pub const MIDI: &str = "midi";
/// Any stage → reporter: a human-readable progress line.
pub const PROGRESS: &str = "progress";
/// Reporter → main: the whole pipeline is finished.
pub const DONE: &str = "done";

/// One separated stem on disk.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Stem {
    /// `vocals`, `drums`, `bass`, `other`, …
    pub name: String,
    /// Absolute path to the stem's `.wav`.
    pub wav: String,
    /// Whether it is worth transcribing to pitched MIDI. `drums` is percussion — basic-pitch
    /// would emit nonsense pitches — so it is skipped.
    pub melodic: bool,
}

/// Published once Demucs has split the input. Empty `stems` means separation failed; the
/// reporter then finishes immediately.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StemsReady {
    /// The track's base name (input filename without extension), for naming outputs.
    pub track: String,
    pub stems: Vec<Stem>,
}

/// The result of transcribing one stem.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum StemMidi {
    /// `name` of the stem and the path to its produced `.mid`.
    Ok { name: String, midi: String },
    /// `name` of the stem and why it failed.
    Failed { name: String, error: String },
}

/// A progress line for the console, tagged with the stage that emitted it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Progress {
    pub stage: String,
    pub msg: String,
}

/// The final tally, handed to `main` so it can print a summary and wind the runtime down.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Done {
    pub midi_files: Vec<String>,
    pub ok: usize,
    pub failed: usize,
}
