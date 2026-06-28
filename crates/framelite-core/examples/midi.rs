//! A tiny MIDI player on framelite: `cargo run -p framelite-core --example midi`
//!
//! Two decoupled modules wired only through channel names:
//!
//!   main ──"transport"──▶ Sequencer ──"note"──▶ Synth
//!
//! * `Sequencer` owns a little melody. On `Transport::Play` it walks the score, publishing
//!   `NoteOn`/`NoteOff` MIDI messages in time. The note duration *is* a `recv_timeout`, so a
//!   `Transport::Stop` arriving mid-note interrupts playback at once — the bus carries both
//!   the clock and the control.
//! * `Synth` knows nothing about the sequencer or the song: it just renders whatever MIDI
//!   messages land on `note`. Swap it for a real cpal/oscillator synth and nothing else moves.

use std::sync::Arc;
use std::time::Duration;

use framelite_bus::LocalBus;
use framelite_core::{Module, ModuleCtx, Runtime};
use framelite_protocol::{Channel, Envelope, ModuleId};
use serde::{Deserialize, Serialize};

/// Typed MIDI messages on the `note` channel — the real voltage of the player.
#[derive(Serialize, Deserialize)]
enum MidiMessage {
    NoteOn { note: u8, velocity: u8 },
    NoteOff { note: u8 },
}

/// Typed control messages on the `transport` channel.
#[derive(Serialize, Deserialize, PartialEq)]
enum Transport {
    Play,
    Stop,
}

/// One musical beat in milliseconds (≈ quarter note). Snappy so the demo is quick.
const MS_PER_BEAT: u64 = 150;
const VELOCITY: u8 = 100;

/// The score: (MIDI note number, length in beats). "Twinkle, twinkle" — first phrase.
fn melody() -> Vec<(u8, u32)> {
    // C4=60  G4=67  A4=69  F4=65  E4=64  D4=62
    vec![
        (60, 1), (60, 1), (67, 1), (67, 1), (69, 1), (69, 1), (67, 2),
        (65, 1), (65, 1), (64, 1), (64, 1), (62, 1), (62, 1), (60, 2),
    ]
}

/// Render a MIDI note number as a name + octave, e.g. 60 → "C4".
fn note_name(n: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    let octave = n as i32 / 12 - 1;
    format!("{}{}", NAMES[(n % 12) as usize], octave)
}

/// Drives the score in time and emits MIDI. Listens on `transport` for Play/Stop.
struct Sequencer;

impl Module for Sequencer {
    fn id(&self) -> ModuleId {
        ModuleId::new("sequencer")
    }
    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new("transport")]
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let score = melody();
        while !ctx.should_stop() {
            // Idle until someone presses Play (waking now and then to observe shutdown).
            match ctx.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(env)) if env.decode::<Transport>().ok() == Some(Transport::Play) => {
                    println!("▶ play");
                    self.play(&ctx, &score);
                }
                Ok(_) => {}      // some other message, or not Play
                Err(_) => break, // bus closed
            }
        }
    }
}

impl Sequencer {
    /// Walk the score. Between note-on and note-off we *wait on the bus*: if a `Stop` arrives
    /// during the note, we cut it short; otherwise the timeout is the note's duration.
    fn play(&self, ctx: &ModuleCtx, score: &[(u8, u32)]) {
        for &(note, beats) in score {
            if ctx.should_stop() {
                return;
            }
            ctx.publish_msg("note", &MidiMessage::NoteOn { note, velocity: VELOCITY })
                .unwrap();

            let dur = Duration::from_millis(beats as u64 * MS_PER_BEAT);
            match ctx.recv_timeout(dur) {
                // Interrupted by Stop: release the sounding note and bail out.
                Ok(Some(env)) if env.decode::<Transport>().ok() == Some(Transport::Stop) => {
                    ctx.publish_msg("note", &MidiMessage::NoteOff { note }).unwrap();
                    println!("■ stop");
                    return;
                }
                _ => {} // timeout = the note's full length elapsed
            }
            ctx.publish_msg("note", &MidiMessage::NoteOff { note }).unwrap();
        }
        println!("□ end of song");
    }
}

/// A "voice": renders whatever MIDI it hears. Tracks how many notes are sounding.
struct Synth;

impl Module for Synth {
    fn id(&self) -> ModuleId {
        ModuleId::new("synth")
    }
    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new("note")]
    }
    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let mut voices = 0i32;
        while !ctx.should_stop() {
            match ctx.recv_timeout(Duration::from_millis(50)) {
                Ok(Some(env)) => match env.decode::<MidiMessage>() {
                    Ok(MidiMessage::NoteOn { note, velocity }) => {
                        voices += 1;
                        println!("  ♪ {:<3} on   vel {:<3} [{} sounding]", note_name(note), velocity, voices);
                    }
                    Ok(MidiMessage::NoteOff { note }) => {
                        voices -= 1;
                        println!("    {:<3} off", note_name(note));
                    }
                    Err(_) => {} // not a MIDI message — ignore
                },
                Ok(None) => {}   // timeout → re-check should_stop
                Err(_) => break, // bus closed
            }
        }
    }
}

fn main() {
    let bus = Arc::new(LocalBus::new());
    let mut rt = Runtime::with_bus(bus.clone());

    rt.spawn(Synth);
    rt.spawn(Sequencer);

    // "Press Play."
    bus.publish(
        Envelope::encode(ModuleId::new("main"), "transport", &Transport::Play).unwrap(),
    )
    .unwrap();

    // Let the phrase play out, then wind everything down cleanly.
    let song_ms: u64 = melody().iter().map(|&(_, b)| b as u64 * MS_PER_BEAT).sum();
    std::thread::sleep(Duration::from_millis(song_ms + 300));

    rt.shutdown();
    let report = rt.join();
    println!("done (clean: {}).", report.is_clean());
}
