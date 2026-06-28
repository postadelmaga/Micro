//! A small, composable **master effects chain** with instrument-flavoured presets.
//!
//! This lives entirely in the *example app*, not in framelite — it is exactly the kind of
//! domain weight (DSP) that an app built on the core brings itself.
//!
//! Honest scope note: rustysynth renders one stereo mix for the whole MIDI file, so an effect
//! here is applied to the *master* output, not to a single instrument. A true per-instrument
//! rig (a compressor only on the bass, a wah only on the guitar) would need per-MIDI-channel
//! rendering and routing; that is beyond an example. So the presets below are voicing presets:
//! pick the one that matches the character you want and it colours the whole output.

use std::f32::consts::{PI, TAU};

/// One mono-linked-stereo DSP stage. Stateful (filters, envelopes), processed per sample.
pub trait Effect: Send {
    fn process(&mut self, l: f32, r: f32) -> (f32, f32);
}

/// An ordered list of effects applied in sequence.
pub struct Chain {
    effects: Vec<Box<dyn Effect>>,
}

impl Chain {
    #[inline]
    pub fn process(&mut self, mut l: f32, mut r: f32) -> (f32, f32) {
        for e in self.effects.iter_mut() {
            (l, r) = e.process(l, r);
        }
        (l, r)
    }
}

// --- effects --------------------------------------------------------------------

/// Feed-forward peak compressor (stereo-linked). Tames dynamics and adds punch/sustain.
struct Compressor {
    threshold: f32, // linear
    ratio: f32,
    attack: f32,  // smoothing coeff
    release: f32, // smoothing coeff
    makeup: f32,  // linear
    env: f32,
}

impl Compressor {
    fn new(sr: f32, threshold_db: f32, ratio: f32, attack_ms: f32, release_ms: f32, makeup_db: f32) -> Self {
        Self {
            threshold: db_to_lin(threshold_db),
            ratio,
            attack: time_coeff(attack_ms, sr),
            release: time_coeff(release_ms, sr),
            makeup: db_to_lin(makeup_db),
            env: 0.0,
        }
    }
}

impl Effect for Compressor {
    fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let level = l.abs().max(r.abs());
        // Fast to clamp transients (attack), slow to let go (release).
        let coeff = if level > self.env { self.attack } else { self.release };
        self.env = coeff * self.env + (1.0 - coeff) * level;

        let gain = if self.env > self.threshold {
            // Above threshold the excess is reduced by the ratio.
            (self.env / self.threshold).powf(1.0 / self.ratio - 1.0)
        } else {
            1.0
        };
        let g = gain * self.makeup;
        (l * g, r * g)
    }
}

/// Waveshaping distortion with a tone (one-pole low-pass) to tame fizz. Drive sets the grit.
struct Distortion {
    drive: f32,
    level: f32,
    tone: f32, // low-pass coeff
    lp_l: f32,
    lp_r: f32,
}

impl Distortion {
    fn new(sr: f32, drive: f32, level: f32, tone_hz: f32) -> Self {
        let tone = 1.0 - (-TAU * tone_hz / sr).exp();
        Self { drive, level, tone, lp_l: 0.0, lp_r: 0.0 }
    }
}

impl Effect for Distortion {
    fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        let yl = (l * self.drive).tanh();
        let yr = (r * self.drive).tanh();
        self.lp_l += self.tone * (yl - self.lp_l);
        self.lp_r += self.tone * (yr - self.lp_r);
        (self.lp_l * self.level, self.lp_r * self.level)
    }
}

/// Auto-wah: a resonant band-pass whose centre frequency is swept by an LFO — the classic
/// "wah-wah" vowel. State-variable (Chamberlin) filter per channel, sharing one LFO.
struct AutoWah {
    sr: f32,
    base: f32,  // Hz, low end of the sweep
    depth: f32, // Hz, sweep span
    q: f32,
    wet: f32,
    lfo_inc: f32,
    lfo_phase: f32,
    low_l: f32,
    band_l: f32,
    low_r: f32,
    band_r: f32,
}

impl AutoWah {
    fn new(sr: f32, rate_hz: f32, base: f32, depth: f32, q: f32, wet: f32) -> Self {
        Self {
            sr,
            base,
            depth,
            q,
            wet,
            lfo_inc: rate_hz / sr,
            lfo_phase: 0.0,
            low_l: 0.0,
            band_l: 0.0,
            low_r: 0.0,
            band_r: 0.0,
        }
    }
}

impl Effect for AutoWah {
    fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        self.lfo_phase += self.lfo_inc;
        if self.lfo_phase >= 1.0 {
            self.lfo_phase -= 1.0;
        }
        // Smooth 0..1 sweep.
        let lfo = 0.5 - 0.5 * (TAU * self.lfo_phase).cos();
        let fc = self.base + self.depth * lfo;
        let f = 2.0 * (PI * fc / self.sr).sin();
        let qr = self.q.recip();

        let high_l = l - self.low_l - qr * self.band_l;
        self.band_l += f * high_l;
        self.low_l += f * self.band_l;

        let high_r = r - self.low_r - qr * self.band_r;
        self.band_r += f * high_r;
        self.low_r += f * self.band_r;

        // The band-pass output is the wah voice; blend it with the dry signal.
        let dry = 1.0 - self.wet;
        (l * dry + self.band_l * self.wet, r * dry + self.band_r * self.wet)
    }
}

// --- presets --------------------------------------------------------------------

/// Instrument-flavoured chains. Selectable from the UI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    Clean,
    Bass,
    GuitarCrunch,
    GuitarLead,
    WahFunk,
}

impl Preset {
    pub fn name(self) -> &'static str {
        match self {
            Preset::Clean => "Clean",
            Preset::Bass => "Bass (comp + warmth)",
            Preset::GuitarCrunch => "Guitar (crunch + wah)",
            Preset::GuitarLead => "Guitar (lead sustain)",
            Preset::WahFunk => "Wah funk",
        }
    }

    /// Pick a sensible preset from a General-MIDI program number (0-based). Channel 10 (drums)
    /// is passed as `is_drums` and left clean. GM groups: 24–31 guitar, 32–39 bass.
    pub fn for_gm_program(program: u8, is_drums: bool) -> Preset {
        if is_drums {
            return Preset::Clean;
        }
        match program {
            32..=39 => Preset::Bass,        // acoustic … synth bass
            28 => Preset::WahFunk,          // muted electric guitar → funk wah
            29 | 30 => Preset::GuitarLead,  // overdriven / distortion guitar
            24..=31 => Preset::GuitarCrunch, // other guitars
            _ => Preset::Clean,
        }
    }

    /// Build the effect chain for this preset at the given sample rate.
    pub fn build(self, sr: f32) -> Chain {
        let effects: Vec<Box<dyn Effect>> = match self {
            Preset::Clean => vec![],
            // Punchy, even bass: compress hard, add a touch of saturation for warmth.
            Preset::Bass => vec![
                Box::new(Compressor::new(sr, -20.0, 4.0, 8.0, 140.0, 4.0)),
                Box::new(Distortion::new(sr, 1.6, 0.95, 3500.0)),
            ],
            // Rhythm guitar: medium grit, then a gentle wah sweep on top.
            Preset::GuitarCrunch => vec![
                Box::new(Distortion::new(sr, 12.0, 0.7, 3000.0)),
                Box::new(AutoWah::new(sr, 1.4, 400.0, 1400.0, 3.5, 0.45)),
            ],
            // Lead: heavier drive, then a compressor for long singing sustain.
            Preset::GuitarLead => vec![
                Box::new(Distortion::new(sr, 26.0, 0.6, 3600.0)),
                Box::new(Compressor::new(sr, -26.0, 3.0, 5.0, 260.0, 5.0)),
            ],
            // Funk: prominent fast wah, kept even by a light compressor.
            Preset::WahFunk => vec![
                Box::new(AutoWah::new(sr, 2.6, 350.0, 1600.0, 4.5, 0.85)),
                Box::new(Compressor::new(sr, -22.0, 3.0, 6.0, 120.0, 3.0)),
            ],
        };
        Chain { effects }
    }
}

// --- helpers --------------------------------------------------------------------

fn db_to_lin(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// One-pole smoothing coefficient for a time constant in milliseconds.
fn time_coeff(ms: f32, sr: f32) -> f32 {
    if ms <= 0.0 {
        0.0
    } else {
        (-1.0 / (ms * 0.001 * sr)).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressor_reduces_a_loud_signal_more_than_a_quiet_one() {
        let sr = 44100.0;
        // Settle each compressor on a steady tone level, then read the applied gain.
        let gain_at = |amp: f32| {
            let mut c = Compressor::new(sr, -20.0, 4.0, 1.0, 1.0, 0.0);
            let mut last = 0.0;
            for _ in 0..4000 {
                let (o, _) = c.process(amp, amp);
                last = o;
            }
            last / amp // effective gain
        };
        // Loud input is attenuated more (lower gain) than a near-threshold one.
        assert!(gain_at(0.5) < gain_at(0.12));
    }

    #[test]
    fn clean_preset_is_bit_exact_bypass() {
        let mut chain = Preset::Clean.build(44100.0);
        assert_eq!(chain.process(0.3, -0.7), (0.3, -0.7));
    }

    #[test]
    fn gm_programs_map_to_expected_presets() {
        assert_eq!(Preset::for_gm_program(33, false), Preset::Bass); // electric bass
        assert_eq!(Preset::for_gm_program(30, false), Preset::GuitarLead); // distortion gtr
        assert_eq!(Preset::for_gm_program(28, false), Preset::WahFunk); // muted gtr
        assert_eq!(Preset::for_gm_program(24, false), Preset::GuitarCrunch); // nylon gtr
        assert_eq!(Preset::for_gm_program(0, false), Preset::Clean); // piano
        assert_eq!(Preset::for_gm_program(33, true), Preset::Clean); // drums override
    }
}
