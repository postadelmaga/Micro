//! Stage 3 — **reporting**. A sink module: it prints every progress line, tracks how many
//! melodic stems are expected, collects each transcription result, and once they have all
//! arrived publishes a [`Done`] so `main` can print the summary and shut the runtime down.

use std::time::Duration;

use framelite_core::{Channel, Module, ModuleCtx, ModuleId};

use crate::messages::{Done, Progress, StemMidi, StemsReady, DONE, MIDI, PROGRESS, STEMS};

#[derive(Default)]
pub struct Reporter;

impl Module for Reporter {
    fn id(&self) -> ModuleId {
        ModuleId::new("reporter")
    }

    fn subscriptions(&self) -> Vec<Channel> {
        vec![
            Channel::new(PROGRESS),
            Channel::new(STEMS),
            Channel::new(MIDI),
        ]
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let mut expected: Option<usize> = None;
        let mut midi_files: Vec<String> = Vec::new();
        let mut ok = 0usize;
        let mut failed = 0usize;

        while !ctx.should_stop() {
            let env = match ctx.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(env)) => env,
                Ok(None) => continue,
                Err(_) => break,
            };

            match env.channel.0.as_str() {
                PROGRESS => {
                    if let Ok(p) = env.decode::<Progress>() {
                        println!("  [{}] {}", p.stage, p.msg);
                    }
                }
                STEMS => {
                    if let Ok(ready) = env.decode::<StemsReady>() {
                        let n = ready.stems.iter().filter(|s| s.melodic).count();
                        expected = Some(n);
                        println!(
                            "  → {} melodic stem(s) to transcribe for \"{}\"",
                            n, ready.track
                        );
                    }
                }
                MIDI => {
                    match env.decode::<StemMidi>() {
                        Ok(StemMidi::Ok { name, midi }) => {
                            ok += 1;
                            println!("  ✓ {name:<7} → {midi}");
                            midi_files.push(midi);
                        }
                        Ok(StemMidi::Failed { name, error }) => {
                            failed += 1;
                            println!("  ✗ {name:<7} {error}");
                        }
                        Err(_) => {}
                    }
                }
                _ => {}
            }

            // Done once every expected transcription has reported (or there were none).
            if let Some(n) = expected {
                if ok + failed >= n {
                    let _ = ctx.publish_msg(
                        DONE,
                        &Done {
                            midi_files: std::mem::take(&mut midi_files),
                            ok,
                            failed,
                        },
                    );
                    break;
                }
            }
        }
    }
}
