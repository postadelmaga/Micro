//! Stage 1 — **separation** (Demucs). A source module: it runs Demucs on the input track,
//! finds the stem `.wav`s it wrote, and publishes a [`StemsReady`]. Demucs is slow (minutes on
//! CPU) and prints its own progress bar, which we let through to the console.

use std::path::{Path, PathBuf};
use std::process::Command;

use framelite_core::{Module, ModuleCtx, ModuleId};

use crate::messages::{Progress, Stem, StemsReady, PROGRESS, STEMS};
use crate::tools;

/// The stems htdemucs (the default 4-stem model) produces. `drums` is percussion → not melodic.
const KNOWN_STEMS: &[(&str, bool)] = &[
    ("vocals", true),
    ("bass", true),
    ("other", true),
    ("drums", false),
];

pub struct Separator {
    input: PathBuf,
    /// Where Demucs writes (`<workdir>/<model>/<track>/<stem>.wav`).
    workdir: PathBuf,
    model: String,
}

impl Separator {
    pub fn new(input: PathBuf, workdir: PathBuf, model: String) -> Self {
        Self {
            input,
            workdir,
            model,
        }
    }
}

impl Module for Separator {
    fn id(&self) -> ModuleId {
        ModuleId::new("separator")
    }

    // A pure source: it produces stems, listens to nothing.

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let track = self
            .input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("track")
            .to_string();

        let progress = |msg: String| {
            let _ = ctx.publish_msg(
                PROGRESS,
                &Progress {
                    stage: "separate".into(),
                    msg,
                },
            );
        };

        progress(format!(
            "running {} on {} (this can take a few minutes)…",
            tools::demucs_bin(),
            self.input.display()
        ));

        // -n MODEL  --out WORKDIR  INPUT  — stderr inherited so Demucs's progress bar shows.
        let status = Command::new(tools::demucs_bin())
            .args(["-n", &self.model, "--out"])
            .arg(&self.workdir)
            .arg(&self.input)
            .status();

        let stems = match status {
            Ok(s) if s.success() => collect_stems(&self.workdir),
            Ok(s) => {
                progress(format!("demucs exited with {s}"));
                Vec::new()
            }
            Err(e) => {
                progress(format!("could not run demucs: {e}"));
                Vec::new()
            }
        };

        if stems.is_empty() {
            progress("no stems produced".into());
        } else {
            progress(format!(
                "separated into {} stems: {}",
                stems.len(),
                stems
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let _ = ctx.publish_msg(STEMS, &StemsReady { track, stems });
    }
}

/// Find the stem wavs Demucs wrote, anywhere under `workdir`, matching known stem names.
fn collect_stems(workdir: &Path) -> Vec<Stem> {
    let mut wavs = Vec::new();
    find_wavs(workdir, &mut wavs);
    let mut stems = Vec::new();
    for wav in wavs {
        let base = wav
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if let Some((name, melodic)) = KNOWN_STEMS.iter().find(|(n, _)| *n == base) {
            stems.push(Stem {
                name: (*name).to_string(),
                wav: wav.to_string_lossy().into_owned(),
                melodic: *melodic,
            });
        }
    }
    // Stable, predictable order (vocals, bass, other, drums).
    stems.sort_by_key(|s| {
        KNOWN_STEMS
            .iter()
            .position(|(n, _)| *n == s.name)
            .unwrap_or(usize::MAX)
    });
    stems
}

fn find_wavs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_wavs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("wav") {
            out.push(path);
        }
    }
}
