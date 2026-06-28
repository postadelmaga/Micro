//! `midi_doctor` — diagnose a Standard MIDI File, apply safe automatic corrections, and
//! (optionally) compare it against a *reference* MIDI exported from a trusted transcription
//! (MuseScore → "Export → MIDI", or a Guitar Pro file run through a `.gp → .mid` converter).
//!
//! The motivating case: the free-MIDI-site copies of a song are all the *same* karaoke file,
//! often with low timing resolution, stuck notes, or wrong pitches. There is no "better file"
//! to download — the fix is to *diagnose* the one you have and *correct* it against an accurate
//! score. This tool is the first half of that pipeline.
//!
//!   cargo run -p framelite-midi-player --example midi_doctor -- <input.mid>
//!   cargo run -p framelite-midi-player --example midi_doctor -- <input.mid> --fix [-o out.mid]
//!   cargo run -p framelite-midi-player --example midi_doctor -- <input.mid> --ref <reference.mid>
//!
//! What it does:
//!   * **report**  — per-track name, channel(s), GM instrument, note count, pitch range, and
//!                   three classes of defect: stuck notes, zero-length notes, overlaps.
//!   * **--fix**   — write a cleaned copy: drop zero-length notes, close stuck notes. These are
//!                   unambiguous corrections that never need a reference. (Pitch/timing fixes
//!                   *do* need a reference and are out of scope for an automatic pass — the diff
//!                   below tells you where to look.)
//!   * **--ref**   — heuristically match each track to a reference track and report how far they
//!                   diverge (note count, pitch range, pitch-class distribution, likely
//!                   transposition). This flags wrong-key / missing-section / wrong-octave
//!                   tracks; it is a *diagnostic*, not a sample-accurate aligner.
//!
//! This is example-app code; framelite's core crates are untouched.

use std::collections::HashMap;

use midly::num::{u4, u7};
use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};

/// A paired note: a `NoteOn`..`NoteOff` span resolved to absolute ticks. Some fields are kept
/// for completeness / future alignment work even though the current passes only read a subset.
#[allow(dead_code)]
struct Note {
    tick: u64,
    dur: u64,
    ch: u8,
    key: u8,
    vel: u8,
}

/// Everything we learn about one track in a single pass.
struct TrackStat {
    idx: usize,
    name: String,
    channels: Vec<u8>,
    /// First `(channel, program)` seen — what the synth will actually play this track as.
    program: Option<(u8, u8)>,
    notes: Vec<Note>,
    /// `NoteOn` with no matching `NoteOff` before end of track — rings forever.
    stuck: usize,
    /// `NoteOff` at the same tick as its `NoteOn` — silent, clutters the file.
    zero_len: usize,
    /// A second `NoteOn` for a (channel,key) already sounding — retrigger / stacked duplicate.
    overlaps: usize,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut input = None;
    let mut reference = None;
    let mut out = None;
    let mut fix = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ref" => {
                reference = args.get(i + 1).cloned();
                i += 2;
            }
            "-o" | "--out" => {
                out = args.get(i + 1).cloned();
                i += 2;
            }
            "--fix" => {
                fix = true;
                i += 1;
            }
            other => {
                if input.is_none() {
                    input = Some(other.to_string());
                }
                i += 1;
            }
        }
    }

    let Some(input) = input else {
        eprintln!(
            "usage: midi_doctor <input.mid> [--fix] [-o out.mid] [--ref reference.mid]\n\
             \n  --fix          write a cleaned copy (drop zero-length notes, close stuck notes)\n  \
             -o <path>      output path for --fix (default: <input>.fixed.mid)\n  \
             --ref <path>   compare against a reference MIDI and report divergence"
        );
        std::process::exit(2);
    };

    let bytes = std::fs::read(&input).unwrap_or_else(|e| {
        eprintln!("cannot read {input}: {e}");
        std::process::exit(1);
    });
    let smf = Smf::parse(&bytes).unwrap_or_else(|e| {
        eprintln!("not a valid MIDI file: {e}");
        std::process::exit(1);
    });

    let ppq = match smf.header.timing {
        Timing::Metrical(t) => t.as_int(),
        Timing::Timecode(..) => 0,
    };
    println!("file:   {input}");
    println!(
        "format: {:?}   tracks: {}   resolution: {} ticks/quarter{}",
        smf.header.format,
        smf.tracks.len(),
        ppq,
        if ppq != 0 && ppq < 192 {
            "  ⚠ low (≥480 recommended for clean quantization)"
        } else {
            ""
        }
    );

    let stats = analyze(&smf);
    report(&stats);

    if let Some(reference) = reference {
        let rbytes = std::fs::read(&reference).unwrap_or_else(|e| {
            eprintln!("cannot read reference {reference}: {e}");
            std::process::exit(1);
        });
        let rsmf = Smf::parse(&rbytes).unwrap_or_else(|e| {
            eprintln!("reference is not a valid MIDI file: {e}");
            std::process::exit(1);
        });
        let rstats = analyze(&rsmf);
        compare(&stats, &rstats, &reference);
    }

    if fix {
        let out = out.unwrap_or_else(|| format!("{input}.fixed.mid"));
        let fixed = repair(&smf);
        fixed.save(&out).unwrap_or_else(|e| {
            eprintln!("cannot write {out}: {e}");
            std::process::exit(1);
        });
        println!("\nwrote cleaned file → {out}");
    }
}

/// One pass over every track: pair notes and tally defects.
fn analyze(smf: &Smf) -> Vec<TrackStat> {
    let mut stats = Vec::new();
    for (idx, track) in smf.tracks.iter().enumerate() {
        let mut name = String::new();
        let mut channels: Vec<u8> = Vec::new();
        let mut program: Option<(u8, u8)> = None;
        let mut notes: Vec<Note> = Vec::new();
        // (channel, key) → (start tick, velocity) for the currently-sounding note.
        let mut active: HashMap<(u8, u8), (u64, u8)> = HashMap::new();
        let (mut stuck, mut zero_len, mut overlaps) = (0usize, 0usize, 0usize);

        let mut tick = 0u64;
        for te in track {
            tick += te.delta.as_int() as u64;
            match &te.kind {
                TrackEventKind::Meta(MetaMessage::TrackName(bytes)) => {
                    name = String::from_utf8_lossy(bytes).trim().to_string();
                }
                TrackEventKind::Midi { channel, message } => {
                    let ch = channel.as_int();
                    if !channels.contains(&ch) {
                        channels.push(ch);
                    }
                    match message {
                        MidiMessage::ProgramChange { program: p } => {
                            if program.is_none() {
                                program = Some((ch, p.as_int()));
                            }
                        }
                        MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                            let k = (ch, key.as_int());
                            if let Some((start, v)) = active.insert(k, (tick, vel.as_int())) {
                                // A new strike before the old note ended: close the old one here.
                                overlaps += 1;
                                notes.push(Note { tick: start, dur: tick - start, ch, key: k.1, vel: v });
                            }
                        }
                        MidiMessage::NoteOff { key, .. }
                        | MidiMessage::NoteOn { key, .. } => {
                            // Either an explicit NoteOff or a NoteOn with velocity 0.
                            let k = (ch, key.as_int());
                            if let Some((start, v)) = active.remove(&k) {
                                let dur = tick - start;
                                if dur == 0 {
                                    zero_len += 1;
                                }
                                notes.push(Note { tick: start, dur, ch, key: k.1, vel: v });
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        stuck += active.len();
        channels.sort_unstable();
        stats.push(TrackStat { idx, name, channels, program, notes, stuck, zero_len, overlaps });
    }
    stats
}

/// Print the per-track diagnostic table.
fn report(stats: &[TrackStat]) {
    println!("\n── tracks ──────────────────────────────────────────────────────────────");
    for s in stats {
        if s.notes.is_empty() && s.name.is_empty() && s.channels.is_empty() {
            println!("  #{:<2} (meta / conductor track — no notes)", s.idx);
            continue;
        }
        let chans = if s.channels.is_empty() {
            "—".to_string()
        } else {
            s.channels.iter().map(|c| (c + 1).to_string()).collect::<Vec<_>>().join(",")
        };
        let instrument = match s.program {
            Some((ch, _)) if ch == 9 => "Drum Kit (perc.)".to_string(),
            Some((_, p)) => gm_name(p).to_string(),
            None if s.channels.contains(&9) => "Drum Kit (perc.)".to_string(),
            None => "— (no program change)".to_string(),
        };
        let (lo, hi) = pitch_range(&s.notes);
        let range = if s.notes.is_empty() {
            "—".to_string()
        } else {
            format!("{}..{}", note_name(lo), note_name(hi))
        };
        let name = if s.name.is_empty() { "(unnamed)" } else { &s.name };
        println!(
            "  #{:<2} {:<28} ch {:<6} {:<22} {:>5} notes  {}",
            s.idx, name, chans, instrument, s.notes.len(), range
        );
        let mut defects = Vec::new();
        if s.stuck > 0 {
            defects.push(format!("{} stuck", s.stuck));
        }
        if s.zero_len > 0 {
            defects.push(format!("{} zero-length", s.zero_len));
        }
        if s.overlaps > 0 {
            defects.push(format!("{} overlaps", s.overlaps));
        }
        if !defects.is_empty() {
            println!("        ⚠ {}", defects.join(", "));
        }
    }

    let total: usize = stats.iter().map(|s| s.notes.len()).sum();
    let stuck: usize = stats.iter().map(|s| s.stuck).sum();
    let zero: usize = stats.iter().map(|s| s.zero_len).sum();
    println!("  ── {total} notes total");
    if stuck + zero > 0 {
        println!("  fixable automatically: {stuck} stuck + {zero} zero-length  →  run with --fix");
    } else {
        println!("  no stuck/zero-length notes — structurally clean.");
    }
}

/// Heuristically match each input track to a reference track and report divergence.
fn compare(input: &[TrackStat], reference: &[TrackStat], ref_path: &str) {
    println!("\n── vs reference: {ref_path} ─────────────────────────────────────────────");
    println!("  (heuristic match by pitch-class profile; a diagnostic, not a sample-accurate aligner)\n");

    let ref_with_notes: Vec<&TrackStat> = reference.iter().filter(|s| !s.notes.is_empty()).collect();
    if ref_with_notes.is_empty() {
        println!("  reference has no notes — nothing to compare.");
        return;
    }

    for s in input.iter().filter(|s| !s.notes.is_empty()) {
        let prof = pitch_class_profile(&s.notes);
        // Match transposition-invariantly (best over all 12 key rotations) so a wrong-key track
        // still finds its true counterpart — then weight by note-count similarity so parts of the
        // same size win ties (a 1600-note drum track shouldn't grab a 400-note guitar part).
        let best = ref_with_notes
            .iter()
            .map(|r| {
                let (shift, sim) = best_rotation(&prof, &pitch_class_profile(&r.notes));
                let count_sim = count_similarity(s.notes.len(), r.notes.len());
                (r, shift, sim, sim * count_sim)
            })
            .max_by(|a, b| a.3.partial_cmp(&b.3).unwrap());
        let name = if s.name.is_empty() { "(unnamed)" } else { &s.name };
        match best {
            Some((r, shift, sim, _)) => {
                let rname = if r.name.is_empty() { "(unnamed)" } else { &r.name };
                let (lo, hi) = pitch_range(&s.notes);
                let (rlo, rhi) = pitch_range(&r.notes);
                println!(
                    "  #{:<2} {:<24} ↔ ref {:<24} similarity {:.0}%",
                    s.idx, name, rname, sim * 100.0
                );
                println!(
                    "        notes {} vs {} ({:+})   range {}..{} vs {}..{}{}",
                    s.notes.len(),
                    r.notes.len(),
                    s.notes.len() as i64 - r.notes.len() as i64,
                    note_name(lo),
                    note_name(hi),
                    note_name(rlo),
                    note_name(rhi),
                    if shift != 0 && sim > 0.85 {
                        format!("   ⚠ looks transposed by {shift:+} semitones")
                    } else {
                        String::new()
                    }
                );
                if sim < 0.6 {
                    println!("        ⚠ low similarity — likely wrong part, wrong key, or missing sections");
                }
            }
            None => println!("  #{:<2} {name}: no reference match", s.idx),
        }
    }
}

/// Produce a cleaned copy: drop zero-length notes, close stuck notes. Nothing else is touched —
/// these two fixes are objectively correct and reference-free.
fn repair<'a>(smf: &Smf<'a>) -> Smf<'a> {
    let mut out = smf.clone();
    for track in &mut out.tracks {
        // Expand to absolute-tick events so we can edit freely, then re-emit deltas.
        let mut abs: Vec<(u64, TrackEventKind<'a>)> = Vec::with_capacity(track.len());
        let mut tick = 0u64;
        let mut end_tick = 0u64;
        for te in track.iter() {
            tick += te.delta.as_int() as u64;
            end_tick = end_tick.max(tick);
            if matches!(te.kind, TrackEventKind::Meta(MetaMessage::EndOfTrack)) {
                continue; // re-added last, after repairs
            }
            abs.push((tick, te.kind));
        }

        // Pair notes to find zero-length pairs (remove both) and stuck NoteOns (synthesize a
        // NoteOff at end_tick). `active` maps (ch,key) → index of the NoteOn in `abs`.
        let mut active: HashMap<(u8, u8), usize> = HashMap::new();
        let mut remove = vec![false; abs.len()];
        let mut to_close: Vec<(u64, u4, u7)> = Vec::new();
        for i in 0..abs.len() {
            let (t, kind) = abs[i];
            if let TrackEventKind::Midi { channel, message } = kind {
                let ch = channel.as_int();
                match message {
                    MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                        active.insert((ch, key.as_int()), i);
                    }
                    MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => {
                        if let Some(on_idx) = active.remove(&(ch, key.as_int())) {
                            if abs[on_idx].0 == t {
                                remove[on_idx] = true;
                                remove[i] = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // Whatever is still active never got a NoteOff: close it at end_tick.
        for ((ch, key), on_idx) in active {
            if !remove[on_idx] {
                to_close.push((end_tick, u4::from(ch), u7::from(key)));
            }
        }

        let mut kept: Vec<(u64, TrackEventKind<'a>)> = abs
            .into_iter()
            .zip(remove)
            .filter(|(_, drop)| !drop)
            .map(|(ev, _)| ev)
            .collect();
        for (t, ch, key) in to_close {
            kept.push((
                t,
                TrackEventKind::Midi {
                    channel: ch,
                    message: MidiMessage::NoteOff { key, vel: u7::from(0) },
                },
            ));
        }
        // Stable sort keeps relative order at equal ticks; re-emit deltas.
        kept.sort_by_key(|(t, _)| *t);
        let mut rebuilt: Vec<TrackEvent<'a>> = Vec::with_capacity(kept.len() + 1);
        let mut prev = 0u64;
        for (t, kind) in kept {
            rebuilt.push(TrackEvent { delta: ((t - prev) as u32).into(), kind });
            prev = t;
        }
        rebuilt.push(TrackEvent {
            delta: ((end_tick.saturating_sub(prev)) as u32).into(),
            kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
        });
        *track = rebuilt;
    }
    out
}

// ── small music helpers ───────────────────────────────────────────────────────

fn pitch_range(notes: &[Note]) -> (u8, u8) {
    let lo = notes.iter().map(|n| n.key).min().unwrap_or(0);
    let hi = notes.iter().map(|n| n.key).max().unwrap_or(0);
    (lo, hi)
}

/// Normalized 12-bin pitch-class histogram (sums to 1), weighted by note duration so a held
/// chord tone counts more than a passing note.
fn pitch_class_profile(notes: &[Note]) -> [f64; 12] {
    let mut h = [0.0f64; 12];
    for n in notes {
        h[(n.key % 12) as usize] += (n.dur.max(1)) as f64;
    }
    let sum: f64 = h.iter().sum();
    if sum > 0.0 {
        for v in &mut h {
            *v /= sum;
        }
    }
    h
}

fn cosine(a: &[f64; 12], b: &[f64; 12]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Best alignment of two pitch-class profiles over all 12 key rotations: returns the shift `k`
/// (folded into -6..=6 semitones) and the cosine similarity at that shift. `k == 0` means the
/// two are already in the same key; a nonzero `k` with high similarity means a transposition.
fn best_rotation(a: &[f64; 12], b: &[f64; 12]) -> (i8, f64) {
    let mut best = (0i8, cosine(a, b));
    for k in 1..12i8 {
        let rotated: [f64; 12] = std::array::from_fn(|i| a[((i as i8 - k).rem_euclid(12)) as usize]);
        let sim = cosine(&rotated, b);
        if sim > best.1 {
            best = (k, sim);
        }
    }
    let k = if best.0 > 6 { best.0 - 12 } else { best.0 };
    (k, best.1)
}

/// How close two note counts are, 0..1 (`min/max`). Keeps the matcher from pairing parts of
/// wildly different size just because their pitch-class shapes happen to rhyme.
fn count_similarity(a: usize, b: usize) -> f64 {
    let (a, b) = (a as f64, b as f64);
    if a == 0.0 && b == 0.0 {
        1.0
    } else {
        a.min(b) / a.max(b)
    }
}

/// Render a MIDI note number as name + octave, e.g. 60 → "C4".
fn note_name(n: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[(n % 12) as usize], n as i32 / 12 - 1)
}

/// General MIDI program number (0-based) → instrument name.
fn gm_name(p: u8) -> &'static str {
    const GM: [&str; 128] = [
        "Acoustic Grand Piano", "Bright Acoustic Piano", "Electric Grand Piano", "Honky-tonk Piano",
        "Electric Piano 1", "Electric Piano 2", "Harpsichord", "Clavi",
        "Celesta", "Glockenspiel", "Music Box", "Vibraphone",
        "Marimba", "Xylophone", "Tubular Bells", "Dulcimer",
        "Drawbar Organ", "Percussive Organ", "Rock Organ", "Church Organ",
        "Reed Organ", "Accordion", "Harmonica", "Tango Accordion",
        "Acoustic Guitar (nylon)", "Acoustic Guitar (steel)", "Electric Guitar (jazz)", "Electric Guitar (clean)",
        "Electric Guitar (muted)", "Overdriven Guitar", "Distortion Guitar", "Guitar Harmonics",
        "Acoustic Bass", "Electric Bass (finger)", "Electric Bass (pick)", "Fretless Bass",
        "Slap Bass 1", "Slap Bass 2", "Synth Bass 1", "Synth Bass 2",
        "Violin", "Viola", "Cello", "Contrabass",
        "Tremolo Strings", "Pizzicato Strings", "Orchestral Harp", "Timpani",
        "String Ensemble 1", "String Ensemble 2", "Synth Strings 1", "Synth Strings 2",
        "Choir Aahs", "Voice Oohs", "Synth Voice", "Orchestra Hit",
        "Trumpet", "Trombone", "Tuba", "Muted Trumpet",
        "French Horn", "Brass Section", "Synth Brass 1", "Synth Brass 2",
        "Soprano Sax", "Alto Sax", "Tenor Sax", "Baritone Sax",
        "Oboe", "English Horn", "Bassoon", "Clarinet",
        "Piccolo", "Flute", "Recorder", "Pan Flute",
        "Blown Bottle", "Shakuhachi", "Whistle", "Ocarina",
        "Lead 1 (square)", "Lead 2 (sawtooth)", "Lead 3 (calliope)", "Lead 4 (chiff)",
        "Lead 5 (charang)", "Lead 6 (voice)", "Lead 7 (fifths)", "Lead 8 (bass + lead)",
        "Pad 1 (new age)", "Pad 2 (warm)", "Pad 3 (polysynth)", "Pad 4 (choir)",
        "Pad 5 (bowed)", "Pad 6 (metallic)", "Pad 7 (halo)", "Pad 8 (sweep)",
        "FX 1 (rain)", "FX 2 (soundtrack)", "FX 3 (crystal)", "FX 4 (atmosphere)",
        "FX 5 (brightness)", "FX 6 (goblins)", "FX 7 (echoes)", "FX 8 (sci-fi)",
        "Sitar", "Banjo", "Shamisen", "Koto",
        "Kalimba", "Bag pipe", "Fiddle", "Shanai",
        "Tinkle Bell", "Agogo", "Steel Drums", "Woodblock",
        "Taiko Drum", "Melodic Tom", "Synth Drum", "Reverse Cymbal",
        "Guitar Fret Noise", "Breath Noise", "Seashore", "Bird Tweet",
        "Telephone Ring", "Helicopter", "Applause", "Gunshot",
    ];
    GM.get(p as usize).copied().unwrap_or("Unknown")
}
