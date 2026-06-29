//! micro-stems — MP3 → instrument stems → per-stem MIDI, with micro as the orchestrator.
//!
//! ```text
//! input.mp3 ─▶ Separator(Demucs) ─stems─▶ Transcriber(basic-pitch, on the worker pool) ─midi─▶ Reporter ─done─▶ main
//! ```
//!
//! micro is the *spine*, not the DSP: Demucs separates, basic-pitch transcribes, both as
//! subprocesses. The audio stays in files those tools exchange on disk; the bus carries only
//! paths and progress. The stems transcribe in parallel on the runtime's worker pool.
//!
//! Usage:
//! ```text
//! micro-stems <input.mp3> [--out DIR] [--model NAME]
//! micro-stems --check          # just verify the external tools are installed
//! ```

mod messages;
mod reporter;
mod separator;
mod tools;
mod transcriber;

use std::path::PathBuf;
use std::process::exit;

use micro_app::App;

use messages::{Done, DONE};

fn main() {
    let args = Args::parse(std::env::args().skip(1));

    // Preflight: the heavy tools must be present before we start a multi-minute job.
    let have_demucs = tools::available(&tools::demucs_bin());
    let have_basic_pitch = tools::available(&tools::basic_pitch_bin());
    if args.check || !have_demucs || !have_basic_pitch {
        report_tools(have_demucs, have_basic_pitch);
        // `--check` is informational; a real run with a missing tool is a hard stop.
        if args.check {
            exit(if have_demucs && have_basic_pitch { 0 } else { 1 });
        }
        if !have_demucs || !have_basic_pitch {
            print_setup_help();
            exit(2);
        }
    }

    let Some(input) = args.input else {
        eprintln!("usage: micro-stems <input.mp3> [--out DIR] [--model NAME]");
        eprintln!("       micro-stems --check");
        exit(64);
    };
    if !input.exists() {
        eprintln!("input not found: {}", input.display());
        exit(66);
    }

    // Output layout: separated stems and the produced MIDI under one --out directory.
    let out = args.out.unwrap_or_else(|| PathBuf::from("stems-out"));
    let sep_dir = out.join("separated");
    let midi_dir = out.join("midi");
    if let Err(e) = std::fs::create_dir_all(&midi_dir).and(std::fs::create_dir_all(&sep_dir)) {
        eprintln!("cannot create output dir {}: {e}", out.display());
        exit(73);
    }

    println!("micro-stems");
    println!("  input : {}", input.display());
    println!("  out   : {}", out.display());
    println!("  model : {}\n", args.model);

    // Wire the pipeline. Reporter + Transcriber subscribe before the Separator runs, so no
    // message is missed; main waits on `done`.
    let mut app = App::new();
    let done_rx = app.bus().subscribe(DONE);
    app.sink(reporter::Reporter);
    app.spawn(transcriber::Transcriber::new(midi_dir));
    app.source(separator::Separator::new(input, sep_dir, args.model));

    // Block until the reporter says every stem has been handled (separation alone is minutes).
    let summary = match done_rx.recv() {
        Ok(env) => env.decode::<Done>().ok(),
        Err(_) => None,
    };

    let report = app.shutdown_and_join();
    if !report.is_clean() {
        eprintln!("modules panicked: {:?}", report.panicked);
    }

    match summary {
        Some(d) if d.ok > 0 => {
            println!("\ndone — {} MIDI file(s), {} failed:", d.ok, d.failed);
            for f in d.midi_files {
                println!("  {f}");
            }
        }
        Some(d) => {
            println!("\nno MIDI produced ({} failed). See the messages above.", d.failed);
            exit(1);
        }
        None => {
            eprintln!("\npipeline ended without a result.");
            exit(1);
        }
    }
}

/// Minimal hand-rolled argument parsing — no clap dependency for a three-flag CLI.
struct Args {
    input: Option<PathBuf>,
    out: Option<PathBuf>,
    model: String,
    check: bool,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Self {
        let mut input = None;
        let mut out = None;
        let mut model = "htdemucs".to_string();
        let mut check = false;
        let mut it = args.peekable();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--check" => check = true,
                "--out" => out = it.next().map(PathBuf::from),
                "--model" => {
                    if let Some(m) = it.next() {
                        model = m;
                    }
                }
                _ if a.starts_with("--") => eprintln!("ignoring unknown flag {a}"),
                _ => input = Some(PathBuf::from(a)),
            }
        }
        Self {
            input,
            out,
            model,
            check,
        }
    }
}

fn report_tools(have_demucs: bool, have_basic_pitch: bool) {
    let mark = |ok: bool| if ok { "✓" } else { "✗ missing" };
    println!("external tools:");
    println!("  demucs       ({})  {}", tools::demucs_bin(), mark(have_demucs));
    println!(
        "  basic-pitch  ({})  {}",
        tools::basic_pitch_bin(),
        mark(have_basic_pitch)
    );
    println!("  ffmpeg       (needed by demucs for mp3) — ensure it is on PATH\n");
}

fn print_setup_help() {
    eprintln!(
        "\nThe ML tools aren't installed. basic-pitch pins an older TensorFlow whose wheels stop\n\
         at CPython 3.11, so the environment must be Python 3.10 or 3.11 (not 3.12+). Plus ffmpeg.\n\
         Easiest with uv (it fetches the right Python):\n\
         \n\
         \x20 uv venv --python 3.11 ~/.venvs/stems\n\
         \x20 VIRTUAL_ENV=~/.venvs/stems uv pip install -U demucs basic-pitch torchcodec\n\
         \x20 # (torchcodec is what current torchaudio uses to write the stem wavs)\n\
         \n\
         Then either activate it (so `demucs`/`basic-pitch` are on PATH) and run micro-stems,\n\
         or point at it explicitly:\n\
         \n\
         \x20 DEMUCS_BIN=~/.venvs/stems/bin/demucs \\\n\
         \x20 BASIC_PITCH_BIN=~/.venvs/stems/bin/basic-pitch \\\n\
         \x20 cargo run -p micro-stems -- song.mp3 --out stems-out\n\
         \n\
         (First run also downloads model weights — a few hundred MB.)\n"
    );
}
