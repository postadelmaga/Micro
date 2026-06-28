//! The audio engine as a framelite [`Module`]. It owns the cpal output stream and a
//! per-MIDI-channel [`Router`] (one synth + one effect chain per channel), and translates
//! `transport` commands into load/play/stop/volume/reverb actions.
//!
//! The cpal callback runs on the audio thread and pulls the already-mixed, already-effected
//! stereo from the shared [`Router`]; the module's own loop only handles commands and
//! republishes `status`. They meet through a small bundle of shared, lock-light state.
//!
//! Realistic sound needs a real General-MIDI SoundFont, so on startup the engine auto-loads one
//! (cache or system path) or downloads a good one on the worker pool with [`ModuleCtx::offload`].

use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use framelite_core::{Module, ModuleCtx};
use framelite_protocol::{Channel, Envelope, ModuleId};
use rustysynth::SoundFont;

use crate::messages::{Status, TransportCmd, STATUS, TRANSPORT};
use crate::reverb::Reverb;
use crate::router::Router;

/// A high-quality, freely redistributable General-MIDI SoundFont (~32 MB).
const SOUNDFONT_URL: &str =
    "https://github.com/mrbumpy409/GeneralUser-GS/raw/main/GeneralUser-GS.sf2";
const SOUNDFONT_FILE: &str = "GeneralUser-GS.sf2";

/// State shared between the module thread, the real-time audio callback, and a download job.
#[derive(Clone)]
struct Shared {
    /// The loaded song with its per-channel rig; `None` until a SoundFont + MIDI are loaded.
    router: Arc<Mutex<Option<Router>>>,
    /// Master volume as `f32` bits, read in the callback (no lock on the audio path).
    volume: Arc<AtomicU32>,
    /// Master reverb amount as `f32` bits, read in the callback.
    reverb_mix: Arc<AtomicU32>,
    /// True while a SoundFont download is in flight.
    downloading: Arc<AtomicBool>,
    /// Download progress, 0..=100.
    pct: Arc<AtomicU32>,
    /// A one-shot note from the download job (e.g. an error), drained by the module loop.
    download_note: Arc<Mutex<Option<String>>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            router: Arc::new(Mutex::new(None)),
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            reverb_mix: Arc::new(AtomicU32::new(0.15f32.to_bits())),
            downloading: Arc::new(AtomicBool::new(false)),
            pct: Arc::new(AtomicU32::new(0)),
            download_note: Arc::new(Mutex::new(None)),
        }
    }
}

/// The framelite module hosting the audio engine.
pub struct AudioEngine;

impl Module for AudioEngine {
    fn id(&self) -> ModuleId {
        ModuleId::new("audio")
    }

    fn subscriptions(&self) -> Vec<Channel> {
        vec![Channel::new(TRANSPORT)]
    }

    fn run(self: Box<Self>, ctx: ModuleCtx) {
        let shared = Shared::new();

        let (stream, sample_rate) = match build_stream(&shared) {
            Ok(s) => s,
            Err(e) => {
                let _ = ctx.publish_msg(
                    STATUS,
                    &Status {
                        message: Some(format!("audio init failed: {e}")),
                        ..Default::default()
                    },
                );
                return;
            }
        };
        if let Err(e) = stream.play() {
            eprintln!("audio stream failed to start: {e}");
        }

        // Song inputs: a SoundFont (shared by all channels) and the raw MIDI bytes.
        let mut sound_font: Option<Arc<SoundFont>> = None;
        let mut midi_bytes: Option<Vec<u8>> = None;
        let mut status = Status::default();

        // Get a realistic SoundFont with no user effort: load an existing one, else download.
        match find_existing_soundfont() {
            Some(path) => match load_soundfont_file(&path.to_string_lossy()) {
                Ok(sf) => {
                    sound_font = Some(sf);
                    status.soundfont = Some(file_name(&path.to_string_lossy()));
                    status.message = Some("soundfont ready — open a MIDI file".into());
                }
                Err(e) => status.message = Some(format!("soundfont error: {e}")),
            },
            None => {
                status.message = Some("fetching a realistic SoundFont…".into());
                start_download(&ctx, &shared);
            }
        }
        let _ = ctx.publish_msg(STATUS, &status);

        while !ctx.should_stop() {
            // 1) Apply one command if any arrived.
            if let Ok(Some(env)) = ctx.recv_timeout(Duration::from_millis(50)) {
                if let Ok(cmd) = env.decode::<TransportCmd>() {
                    handle(&cmd, &ctx, &shared, sample_rate, &mut sound_font, &mut midi_bytes, &mut status);
                }
            }

            // 2) Read accurate position and notice when the song ends.
            if let Some(r) = shared.router.lock().unwrap().as_mut() {
                status.position = r.position_secs() as f32;
                if r.is_playing() && r.is_finished() {
                    r.stop();
                    status.playing = false;
                    status.message = Some("finished".into());
                }
            }

            // 3) Surface download progress / its final note over the steady status messages.
            if shared.downloading.load(Ordering::Relaxed) {
                status.message = Some(format!(
                    "downloading SoundFont… {}%",
                    shared.pct.load(Ordering::Relaxed)
                ));
            } else if let Some(note) = shared.download_note.lock().unwrap().take() {
                status.message = Some(note);
            }

            // 4) Republish live status (retained → a late UI re-syncs instantly).
            let _ = ctx.publish_msg(STATUS, &status);
        }
        // `stream` is dropped here, on shutdown, stopping the audio device cleanly.
    }
}

/// Apply a single transport command to the engine and update `status` in place.
#[allow(clippy::too_many_arguments)]
fn handle(
    cmd: &TransportCmd,
    ctx: &ModuleCtx,
    shared: &Shared,
    sample_rate: u32,
    sound_font: &mut Option<Arc<SoundFont>>,
    midi_bytes: &mut Option<Vec<u8>>,
    status: &mut Status,
) {
    match cmd {
        TransportCmd::LoadSoundFont(path) => match load_soundfont_file(path) {
            Ok(sf) => {
                *sound_font = Some(sf);
                status.soundfont = Some(file_name(path));
                status.message = Some("soundfont loaded".into());
                rebuild_router(sound_font, midi_bytes, sample_rate, shared, status);
            }
            Err(e) => status.message = Some(format!("soundfont error: {e}")),
        },
        TransportCmd::DownloadSoundFont => start_download(ctx, shared),
        TransportCmd::LoadMidi(path) => match std::fs::read(path) {
            Ok(bytes) => {
                *midi_bytes = Some(bytes);
                status.midi = Some(file_name(path));
                rebuild_router(sound_font, midi_bytes, sample_rate, shared, status);
            }
            Err(e) => status.message = Some(format!("midi read error: {e}")),
        },
        TransportCmd::Play => {
            let mut guard = shared.router.lock().unwrap();
            match guard.as_mut() {
                Some(r) => {
                    r.play();
                    status.playing = true;
                    status.message = Some("playing".into());
                }
                None => {
                    status.message = Some(
                        if sound_font.is_none() {
                            "soundfont still loading…"
                        } else {
                            "load a midi file first"
                        }
                        .into(),
                    )
                }
            }
        }
        TransportCmd::Stop => {
            if let Some(r) = shared.router.lock().unwrap().as_mut() {
                r.stop();
            }
            status.playing = false;
            status.message = Some("stopped".into());
        }
        TransportCmd::SetVolume(v) => {
            let v = v.clamp(0.0, 1.0);
            shared.volume.store(v.to_bits(), Ordering::Relaxed);
            status.volume = v;
        }
        TransportCmd::SetReverb(v) => {
            let v = v.clamp(0.0, 0.6);
            shared.reverb_mix.store(v.to_bits(), Ordering::Relaxed);
            status.reverb = v;
        }
    }
}

/// (Re)build the per-channel router whenever both a SoundFont and a MIDI file are available.
fn rebuild_router(
    sound_font: &Option<Arc<SoundFont>>,
    midi_bytes: &Option<Vec<u8>>,
    sample_rate: u32,
    shared: &Shared,
    status: &mut Status,
) {
    let (Some(sf), Some(bytes)) = (sound_font, midi_bytes) else {
        return;
    };
    match Router::new(sf.clone(), bytes, sample_rate) {
        Ok(router) => {
            status.duration = router.length_secs() as f32;
            status.routing = format_routing(router.routing());
            status.position = 0.0;
            status.playing = false;
            *shared.router.lock().unwrap() = Some(router);
            status.message = Some("ready — press Play".into());
        }
        Err(e) => status.message = Some(format!("midi error: {e}")),
    }
}

fn format_routing(routing: &[(u8, &'static str)]) -> String {
    if routing.is_empty() {
        return "—".into();
    }
    routing
        .iter()
        .map(|(ch, preset)| format!("ch{ch}: {preset}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Kick off a SoundFont download on the runtime's worker pool (so the receive loop keeps
/// draining). On success the job re-enters the bus as a `LoadSoundFont` command.
fn start_download(ctx: &ModuleCtx, shared: &Shared) {
    if shared.downloading.swap(true, Ordering::Relaxed) {
        return; // one already running
    }
    shared.pct.store(0, Ordering::Relaxed);

    let bus = ctx.bus();
    let id = ctx.id().clone();
    let dest = cache_sf2_path();
    let downloading = shared.downloading.clone();
    let pct = shared.pct.clone();
    let note = shared.download_note.clone();

    ctx.offload(move || {
        let result = stream_to_file(SOUNDFONT_URL, &dest, &pct);
        downloading.store(false, Ordering::Relaxed);
        match result {
            Ok(()) => {
                let path = dest.to_string_lossy().into_owned();
                if let Ok(env) = Envelope::encode(id, TRANSPORT, &TransportCmd::LoadSoundFont(path)) {
                    let _ = bus.publish(env);
                }
            }
            Err(e) => *note.lock().unwrap() = Some(format!("download failed: {e}")),
        }
    });
}

/// Download `url` to `dest` (via a `.part` temp file), updating `pct` (0..=100) as it goes.
fn stream_to_file(url: &str, dest: &PathBuf, pct: &AtomicU32) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let resp = ureq::get(url).call().map_err(|e| e.to_string())?;
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let tmp = dest.with_extension("part");
    let mut file = File::create(&tmp).map_err(|e| e.to_string())?;
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        if let Some(p) = (done * 100).checked_div(total) {
            pct.store(p.min(100) as u32, Ordering::Relaxed);
        }
    }
    file.flush().ok();
    drop(file);
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(())
}

/// Load a SoundFont file into a shareable handle (all channel synths reference the same one).
fn load_soundfont_file(path: &str) -> Result<Arc<SoundFont>, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let sound_font = SoundFont::new(&mut f).map_err(|e| e.to_string())?;
    Ok(Arc::new(sound_font))
}

/// Where a downloaded SoundFont is cached: `$XDG_CACHE_HOME/framelite-midi-player/…`.
fn cache_sf2_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("framelite-midi-player").join(SOUNDFONT_FILE)
}

/// The cached download, or the first `.sf2` found in a common system soundfont directory.
fn find_existing_soundfont() -> Option<PathBuf> {
    let cached = cache_sf2_path();
    if cached.is_file() {
        return Some(cached);
    }
    for dir in ["/usr/share/soundfonts", "/usr/share/sounds/sf2"] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("sf2") {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Smooth saturating limiter: nearly transparent at low levels, gently bounds peaks into
/// (-1, 1) with an analog-like curve instead of harsh digital clipping on loud passages.
#[inline]
fn soft_clip(x: f32) -> f32 {
    x.tanh()
}

/// Open the default output device and build a stream for its sample format.
fn build_stream(shared: &Shared) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no output audio device")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let sample_format = config.sample_format();
    let sample_rate = config.sample_rate().0;
    let config: cpal::StreamConfig = config.into();

    let stream = match sample_format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, shared)?,
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, shared)?,
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, shared)?,
        other => return Err(format!("unsupported sample format: {other:?}")),
    };
    Ok((stream, sample_rate))
}

/// Build the output stream for sample type `T`. The callback pulls the router's per-channel
/// mix, applies master volume + reverb + a soft-clip limiter, and interleaves to the device.
fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: &Shared,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let router = shared.router.clone();
    let volume = shared.volume.clone();
    let reverb_mix = shared.reverb_mix.clone();

    // Reused across callbacks so the audio path does no per-call allocation after warm-up.
    let mut left: Vec<f32> = Vec::new();
    let mut right: Vec<f32> = Vec::new();
    // Master reverb — its state (delay lines) must persist across callbacks.
    let mut reverb = Reverb::new(config.sample_rate.0);

    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let frame_count = data.len() / channels.max(1);
                left.resize(frame_count, 0.0);
                right.resize(frame_count, 0.0);

                match router.lock().unwrap().as_mut() {
                    Some(r) => r.render(&mut left, &mut right),
                    None => {
                        left.iter_mut().for_each(|v| *v = 0.0);
                        right.iter_mut().for_each(|v| *v = 0.0);
                    }
                }

                let vol = f32::from_bits(volume.load(Ordering::Relaxed));
                let mix = f32::from_bits(reverb_mix.load(Ordering::Relaxed));
                for (i, frame) in data.chunks_mut(channels).enumerate() {
                    // router (per-channel synth+fx) → master volume → reverb → soft-clip.
                    let (mut l, mut r) = reverb.process(left[i] * vol, right[i] * vol, mix);
                    l = soft_clip(l);
                    r = soft_clip(r);
                    for (ch, sample) in frame.iter_mut().enumerate() {
                        let v = match ch {
                            0 => l,
                            1 => r,
                            _ => 0.5 * (l + r),
                        };
                        *sample = T::from_sample(v);
                    }
                }
            },
            |err| eprintln!("audio stream error: {err}"),
            None,
        )
        .map_err(|e| e.to_string())
}
