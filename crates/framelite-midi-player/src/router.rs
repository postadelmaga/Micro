//! Per-MIDI-channel routing: a custom sequencer that gives **each MIDI channel its own
//! synthesizer and its own effect chain**, so a bass channel gets a compressor and a guitar
//! channel gets distortion + wah — independently, the way a real multitrack rig works.
//!
//! Why a custom sequencer at all? rustysynth's `MidiFileSequencer` drives a *single* synth and
//! mixes all 16 channels internally, and its `MidiFile` doesn't expose the timed events. So we
//! parse the Standard MIDI File ourselves with `midly`, build a sample-accurate timeline (with
//! the tempo map), and dispatch each event to the synth of its channel. All channel synths
//! share one `Arc<SoundFont>`, so there is no soundfont duplication — only per-voice state.
//!
//! This whole file is example-app DSP/host code; framelite's core crates are untouched.

use std::sync::Arc;

use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};

use crate::effects::{Chain, Preset};

/// Sub-block size for dispatch: events are quantised to this many frames (~1.5 ms at 44.1 kHz),
/// matching rustysynth's own internal block granularity.
const RENDER_STEP: usize = 64;
/// Extra time after the last event so final notes ring out before we report "finished".
const TAIL_SECONDS: f64 = 2.0;
/// Per-channel polyphony. One channel rarely needs the global 256; 128 is ample and lean ×16.
const PER_CHANNEL_POLYPHONY: usize = 128;

/// A channel-voice MIDI event, pre-resolved to an absolute sample index.
#[derive(Clone, Copy)]
struct Ev {
    sample: u64,
    channel: u8,
    command: i32,
    data1: i32,
    data2: i32,
}

/// One MIDI channel: its dedicated synth and the effect chain auto-assigned to it.
struct Voice {
    synth: Synthesizer,
    chain: Chain,
}

/// A loaded song, ready to render with per-channel effects.
pub struct Router {
    sample_rate: u32,
    events: Vec<Ev>,
    voices: Vec<Voice>,
    /// MIDI channel (0..16) → index into `voices`, or `None` if the channel is unused.
    chan_to_voice: [Option<usize>; 16],
    /// Human-readable (1-based channel, preset name) for the UI.
    routing: Vec<(u8, &'static str)>,
    pos: u64,
    idx: usize,
    total: u64,
    playing: bool,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
}

impl Router {
    /// Parse `midi_bytes`, lay out a per-channel rig against `sound_font`, ready to play.
    pub fn new(sound_font: Arc<SoundFont>, midi_bytes: &[u8], sample_rate: u32) -> Result<Router, String> {
        let smf = Smf::parse(midi_bytes).map_err(|e| e.to_string())?;
        let tpq = match smf.header.timing {
            Timing::Metrical(t) => t.as_int() as f64,
            _ => 480.0, // SMPTE-timed files are rare; assume a sane PPQ
        };

        // One pass over every track: collect encoded channel events (with absolute ticks),
        // tempo changes, which channels are active, and each channel's first program.
        let mut voice_enc: Vec<(u64, u8, i32, i32, i32)> = Vec::new();
        let mut tempo_raw: Vec<(u64, u32)> = Vec::new();
        let mut active = [false; 16];
        let mut first_program = [None::<u8>; 16];

        for track in &smf.tracks {
            let mut tick = 0u64;
            for te in track {
                tick += te.delta.as_int() as u64;
                match &te.kind {
                    TrackEventKind::Midi { channel, message } => {
                        let ch = channel.as_int();
                        active[ch as usize] = true;
                        if let MidiMessage::ProgramChange { program } = message {
                            if first_program[ch as usize].is_none() {
                                first_program[ch as usize] = Some(program.as_int());
                            }
                        }
                        if let Some((cmd, d1, d2)) = encode(message) {
                            voice_enc.push((tick, ch, cmd, d1, d2));
                        }
                    }
                    TrackEventKind::Meta(MetaMessage::Tempo(us)) => {
                        tempo_raw.push((tick, us.as_int()));
                    }
                    _ => {}
                }
            }
        }

        // Build tempo segments (start_tick, seconds_at_start, micros_per_beat) so any tick can
        // be converted to seconds across tempo changes.
        tempo_raw.sort_by_key(|x| x.0);
        let segs = build_tempo_segments(&tempo_raw, tpq);

        let sr = sample_rate as f64;
        let mut events: Vec<Ev> = voice_enc
            .into_iter()
            .map(|(tick, channel, command, data1, data2)| Ev {
                sample: (tick_to_sec(&segs, tpq, tick) * sr) as u64,
                channel,
                command,
                data1,
                data2,
            })
            .collect();
        events.sort_by_key(|e| e.sample);

        // One synth + one auto-assigned chain per active channel; all share the soundfont.
        let mut settings = SynthesizerSettings::new(sample_rate as i32);
        settings.maximum_polyphony = PER_CHANNEL_POLYPHONY;
        settings.enable_reverb_and_chorus = false; // the master Freeverb provides space, ×16-lean

        let mut voices = Vec::new();
        let mut chan_to_voice = [None; 16];
        let mut routing = Vec::new();
        for ch in 0..16u8 {
            if !active[ch as usize] {
                continue;
            }
            let is_drums = ch == 9; // GM channel 10 (0-based 9) is percussion
            let program = first_program[ch as usize].unwrap_or(0);
            let preset = Preset::for_gm_program(program, is_drums);
            let synth = Synthesizer::new(&sound_font, &settings).map_err(|e| e.to_string())?;
            chan_to_voice[ch as usize] = Some(voices.len());
            routing.push((ch + 1, preset.name()));
            voices.push(Voice {
                synth,
                chain: preset.build(sample_rate as f32),
            });
        }

        let last = events.last().map(|e| e.sample).unwrap_or(0);
        let total = last + (TAIL_SECONDS * sr) as u64;

        Ok(Router {
            sample_rate,
            events,
            voices,
            chan_to_voice,
            routing,
            pos: 0,
            idx: 0,
            total,
            playing: false,
            buf_l: vec![0.0; RENDER_STEP],
            buf_r: vec![0.0; RENDER_STEP],
        })
    }

    pub fn play(&mut self) {
        for v in &mut self.voices {
            v.synth.reset();
        }
        self.pos = 0;
        self.idx = 0;
        self.playing = true;
    }

    pub fn stop(&mut self) {
        self.playing = false;
        for v in &mut self.voices {
            v.synth.note_off_all(false);
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn is_finished(&self) -> bool {
        self.pos >= self.total
    }

    pub fn position_secs(&self) -> f64 {
        self.pos as f64 / self.sample_rate as f64
    }

    pub fn length_secs(&self) -> f64 {
        self.total as f64 / self.sample_rate as f64
    }

    /// (1-based channel, preset name) for each active channel — for the UI.
    pub fn routing(&self) -> &[(u8, &'static str)] {
        &self.routing
    }

    /// Render the next block: dispatch due events, render each channel, apply its chain, sum.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        left.iter_mut().for_each(|x| *x = 0.0);
        right.iter_mut().for_each(|x| *x = 0.0);
        if !self.playing {
            return;
        }

        let frames = left.len();
        let mut off = 0;
        while off < frames {
            let n = RENDER_STEP.min(frames - off);

            // Fire every event whose time has arrived.
            while self.idx < self.events.len() && self.events[self.idx].sample <= self.pos {
                let e = self.events[self.idx];
                if let Some(vi) = self.chan_to_voice[e.channel as usize] {
                    self.voices[vi]
                        .synth
                        .process_midi_message(e.channel as i32, e.command, e.data1, e.data2);
                }
                self.idx += 1;
            }

            // Render each channel into scratch, run its chain, accumulate into the master.
            for v in &mut self.voices {
                v.synth.render(&mut self.buf_l[..n], &mut self.buf_r[..n]);
                for i in 0..n {
                    let (l, r) = v.chain.process(self.buf_l[i], self.buf_r[i]);
                    left[off + i] += l;
                    right[off + i] += r;
                }
            }

            self.pos += n as u64;
            off += n;
        }
    }
}

/// Encode a midly channel message into rustysynth's (command, data1, data2) triple.
fn encode(m: &MidiMessage) -> Option<(i32, i32, i32)> {
    let v = match m {
        MidiMessage::NoteOff { key, vel } => (0x80, key.as_int() as i32, vel.as_int() as i32),
        MidiMessage::NoteOn { key, vel } => (0x90, key.as_int() as i32, vel.as_int() as i32),
        MidiMessage::Aftertouch { key, vel } => (0xA0, key.as_int() as i32, vel.as_int() as i32),
        MidiMessage::Controller { controller, value } => {
            (0xB0, controller.as_int() as i32, value.as_int() as i32)
        }
        MidiMessage::ProgramChange { program } => (0xC0, program.as_int() as i32, 0),
        MidiMessage::ChannelAftertouch { vel } => (0xD0, vel.as_int() as i32, 0),
        MidiMessage::PitchBend { bend } => {
            let raw = bend.0.as_int() as i32; // 14-bit, 0..16383
            (0xE0, raw & 0x7F, (raw >> 7) & 0x7F)
        }
    };
    Some(v)
}

/// (start_tick, seconds_at_start, micros_per_beat) segments, in tick order.
fn build_tempo_segments(tempo_raw: &[(u64, u32)], tpq: f64) -> Vec<(u64, f64, f64)> {
    let mut segs: Vec<(u64, f64, f64)> = vec![(0, 0.0, 500_000.0)]; // default 120 BPM
    let mut cur_tick = 0u64;
    let mut cur_sec = 0.0f64;
    let mut cur_us = 500_000.0f64;
    for &(t, us) in tempo_raw {
        if t == 0 {
            cur_us = us as f64;
            segs[0].2 = cur_us;
            continue;
        }
        cur_sec += (t - cur_tick) as f64 / tpq * (cur_us / 1_000_000.0);
        cur_tick = t;
        cur_us = us as f64;
        segs.push((t, cur_sec, cur_us));
    }
    segs
}

fn tick_to_sec(segs: &[(u64, f64, f64)], tpq: f64, tick: u64) -> f64 {
    // The last segment that starts at or before `tick`.
    let mut seg = &segs[0];
    for s in segs {
        if s.0 <= tick {
            seg = s;
        } else {
            break;
        }
    }
    seg.1 + (tick - seg.0) as f64 / tpq * (seg.2 / 1_000_000.0)
}
