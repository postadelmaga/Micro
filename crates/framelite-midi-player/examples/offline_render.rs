//! Headless sanity check of the audio path: load sf2 + mid, render a few seconds, print RMS.
//! `cargo run -p framelite-midi-player --example offline_render -- <sf2> <mid>`
use std::fs::File;
use std::sync::Arc;

use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (sf2, mid) = (&args[1], &args[2]);
    let sr = 44100;

    let sound_font = Arc::new(SoundFont::new(&mut File::open(sf2).unwrap()).unwrap());
    let midi = Arc::new(MidiFile::new(&mut File::open(mid).unwrap()).unwrap());
    println!("midi length = {:.2}s", midi.get_length());

    let synth = Synthesizer::new(&sound_font, &SynthesizerSettings::new(sr)).unwrap();
    let mut seq = MidiFileSequencer::new(synth);
    seq.play(&midi, false);

    let block = 4410; // 0.1s blocks
    let mut left = vec![0.0f32; block];
    let mut right = vec![0.0f32; block];
    let mut peak = 0.0f32;
    let mut sumsq = 0.0f64;
    let mut n = 0u64;
    for b in 0..50 {
        // 5 seconds
        seq.render(&mut left, &mut right);
        for i in 0..block {
            let v = left[i].abs().max(right[i].abs());
            peak = peak.max(v);
            sumsq += (left[i] as f64).powi(2) + (right[i] as f64).powi(2);
            n += 2;
        }
        if b == 10 {
            println!("eos@1s = {}", seq.end_of_sequence());
        }
    }
    let rms = (sumsq / n as f64).sqrt();
    println!("peak = {peak:.4}  rms = {rms:.5}  eos = {}", seq.end_of_sequence());
    println!(
        "{}",
        if peak > 0.001 { "AUDIO OK ✔ produces sound" } else { "SILENT �’— something is wrong" }
    );
}
