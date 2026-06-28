//! A compact stereo **Freeverb** master reverb (Schroeder/Moorer style: parallel comb filters
//! into series allpass filters). It runs on the master bus *after* the synth, as a send: the
//! dry signal is preserved and a `mix` of reverberant tail is added on top. This adds room/hall
//! space the listener can dial in, on top of rustysynth's per-voice reverb.
//!
//! Buffer lengths are the classic Freeverb tunings, scaled to the actual sample rate.

const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: usize = 23;
const FIXED_GAIN: f32 = 0.015;

/// A lowpass-feedback comb filter — the body of the reverb tail.
struct Comb {
    buf: Vec<f32>,
    idx: usize,
    store: f32,
    damp1: f32,
    damp2: f32,
    feedback: f32,
}

impl Comb {
    fn new(size: usize, feedback: f32, damp: f32) -> Self {
        Self {
            buf: vec![0.0; size.max(1)],
            idx: 0,
            store: 0.0,
            damp1: damp,
            damp2: 1.0 - damp,
            feedback,
        }
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let out = self.buf[self.idx];
        self.store = out * self.damp2 + self.store * self.damp1;
        self.buf[self.idx] = input + self.store * self.feedback;
        self.idx += 1;
        if self.idx >= self.buf.len() {
            self.idx = 0;
        }
        out
    }
}

/// An allpass filter — smears the comb output to thicken the tail without coloring it.
struct Allpass {
    buf: Vec<f32>,
    idx: usize,
    feedback: f32,
}

impl Allpass {
    fn new(size: usize, feedback: f32) -> Self {
        Self {
            buf: vec![0.0; size.max(1)],
            idx: 0,
            feedback,
        }
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let buffered = self.buf[self.idx];
        let out = -input + buffered;
        self.buf[self.idx] = input + buffered * self.feedback;
        self.idx += 1;
        if self.idx >= self.buf.len() {
            self.idx = 0;
        }
        out
    }
}

/// Stereo Freeverb. Construct once per stream; call [`Reverb::process`] per sample.
pub struct Reverb {
    comb_l: Vec<Comb>,
    comb_r: Vec<Comb>,
    allpass_l: Vec<Allpass>,
    allpass_r: Vec<Allpass>,
}

impl Reverb {
    pub fn new(sample_rate: u32) -> Self {
        let scale = sample_rate as f32 / 44100.0;
        let room = 0.82; // longer tail = bigger room
        let damp = 0.25; // gentle high-frequency damping
        let feedback = room * 0.28 + 0.7;
        let len = |t: usize| ((t as f32) * scale) as usize;

        Self {
            comb_l: COMB_TUNING.iter().map(|&t| Comb::new(len(t), feedback, damp)).collect(),
            comb_r: COMB_TUNING
                .iter()
                .map(|&t| Comb::new(len(t + STEREO_SPREAD), feedback, damp))
                .collect(),
            allpass_l: ALLPASS_TUNING.iter().map(|&t| Allpass::new(len(t), 0.5)).collect(),
            allpass_r: ALLPASS_TUNING
                .iter()
                .map(|&t| Allpass::new(len(t + STEREO_SPREAD), 0.5))
                .collect(),
        }
    }

    /// Add `mix` (0..~0.6) worth of reverb tail to a dry stereo sample. The dry signal is kept
    /// at full level, so this behaves as a reverb *send*. `mix <= 0` is a true bypass.
    #[inline]
    pub fn process(&mut self, dry_l: f32, dry_r: f32, mix: f32) -> (f32, f32) {
        if mix <= 0.0 {
            return (dry_l, dry_r);
        }
        let input = (dry_l + dry_r) * FIXED_GAIN;
        let mut wet_l = 0.0;
        let mut wet_r = 0.0;
        for c in self.comb_l.iter_mut() {
            wet_l += c.process(input);
        }
        for c in self.comb_r.iter_mut() {
            wet_r += c.process(input);
        }
        for a in self.allpass_l.iter_mut() {
            wet_l = a.process(wet_l);
        }
        for a in self.allpass_r.iter_mut() {
            wet_r = a.process(wet_r);
        }
        (dry_l + wet_l * mix, dry_r + wet_r * mix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverb_produces_a_decaying_tail_after_the_input_stops() {
        let mut rv = Reverb::new(44100);
        // One impulse, then silence.
        rv.process(1.0, 1.0, 0.3);
        let mut tail = 0.0f32;
        for _ in 0..20_000 {
            let (l, r) = rv.process(0.0, 0.0, 0.3);
            tail += l.abs() + r.abs();
        }
        // The dry input was silent, so any energy here is the reverb tail.
        assert!(tail > 0.01, "expected an audible reverb tail, got {tail}");
    }

    #[test]
    fn zero_mix_is_a_clean_bypass() {
        let mut rv = Reverb::new(44100);
        let (l, r) = rv.process(0.5, -0.3, 0.0);
        assert_eq!((l, r), (0.5, -0.3));
    }
}
