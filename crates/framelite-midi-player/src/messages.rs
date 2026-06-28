//! The typed messages the two sides of the app exchange over the bus. The UI and the audio
//! engine never reference each other — only these types and the channel names.

use serde::{Deserialize, Serialize};

/// Channel the UI publishes user intent on (events).
pub const TRANSPORT: &str = "transport";
/// Channel the audio engine publishes playback state on (retained, so a late UI re-syncs).
pub const STATUS: &str = "status";

/// A command from the window's buttons/sliders to the audio engine.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum TransportCmd {
    /// Path to a `.sf2` SoundFont — required before any sound can be produced.
    LoadSoundFont(String),
    /// Fetch a good General-MIDI SoundFont from the internet into the cache and load it.
    DownloadSoundFont,
    /// Path to a `.mid` file to play.
    LoadMidi(String),
    Play,
    Stop,
    /// Master volume, 0.0..=1.0.
    SetVolume(f32),
    /// Master reverb amount (send), 0.0..~0.6.
    SetReverb(f32),
}

/// The audio engine's view of the world, rendered by the window each frame.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Status {
    pub soundfont: Option<String>,
    pub midi: Option<String>,
    pub playing: bool,
    pub volume: f32,
    /// Master reverb amount (send).
    pub reverb: f32,
    /// Per-MIDI-channel preset assignment, one "chN: Preset" line per active channel.
    pub routing: String,
    /// Seconds elapsed in the current playback.
    pub position: f32,
    /// Total length of the loaded MIDI, in seconds.
    pub duration: f32,
    /// Last human-readable event ("playing", "load a soundfont first", an error…).
    pub message: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            soundfont: None,
            midi: None,
            playing: false,
            volume: 1.0,
            reverb: 0.15,
            routing: "—".to_string(),
            position: 0.0,
            duration: 0.0,
            message: None,
        }
    }
}
