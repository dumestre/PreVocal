//! Freeverb-style stereo reverb (Schroeder architecture).
//!
//! 8 damped comb filters + 4 allpass filters per channel, with the classic
//! Freeverb stereo spread (the right channel's comb delays are offset by
//! 23 samples). Damping is a one-pole lowpass in each comb's feedback loop;
//! `size` sets the comb feedback (0.70..0.98) and `damping` the lowpass
//! coefficient. The wet signal is added back to the dry input via `mix`.

const COMB_LEN_L: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const COMB_LEN_R: [usize; 8] = [1139, 1211, 1300, 1379, 1445, 1514, 1580, 1640];
const ALLPASS_LEN: [usize; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: usize = 23;
const FIXED_GAIN: f32 = 0.015;
const ALLPASS_FEEDBACK: f32 = 0.5;
const TUNING_SAMPLE_RATE: f32 = 44_100.0;

/// Block-invariant reverb settings, pre-computed once per audio block.
#[derive(Clone, Copy)]
pub struct ReverbCoefs {
    comb_feedback: f32,
    damp1: f32,
    damp2: f32,
    mix: f32,
}

/// One comb filter: ring buffer + damping filter state.
struct Comb {
    buffer: Vec<f32>,
    idx: usize,
    store: f32,
}

/// One allpass filter: ring buffer + write position.
struct Allpass {
    buffer: Vec<f32>,
    idx: usize,
}

/// Shared filter state; (re)allocated when the sample rate changes.
#[derive(Default)]
pub struct ReverbState {
    combs_l: Vec<Comb>,
    combs_r: Vec<Comb>,
    allpasses_l: Vec<Allpass>,
    allpasses_r: Vec<Allpass>,
    sample_rate: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self {
            buffer: vec![0.0; len],
            idx: 0,
            store: 0.0,
        }
    }
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self {
            buffer: vec![0.0; len],
            idx: 0,
        }
    }
}

impl ReverbState {
    /// (Re)allocate all filter buffers when the sample rate changes.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if (sample_rate - self.sample_rate).abs() > 0.5 {
            self.sample_rate = sample_rate;
            let scale = (sample_rate / TUNING_SAMPLE_RATE).max(0.5);
            let scale_len = |len: usize| (len as f32 * scale).round() as usize;
            self.combs_l = COMB_LEN_L.iter().map(|&l| Comb::new(scale_len(l))).collect();
            self.combs_r = COMB_LEN_R
                .iter()
                .map(|&l| Comb::new(scale_len(l + STEREO_SPREAD)))
                .collect();
            self.allpasses_l = ALLPASS_LEN.iter().map(|&l| Allpass::new(scale_len(l))).collect();
            self.allpasses_r = ALLPASS_LEN.iter().map(|&l| Allpass::new(scale_len(l))).collect();
        }
    }
}

impl ReverbCoefs {
    pub fn new(size: f32, damping: f32, mix_pct: f32) -> Self {
        let size = size.clamp(0.0, 1.0);
        let damping = damping.clamp(0.0, 1.0);
        // Classic Freeverb mapping: roomsize -> feedback 0.70..0.98,
        // damping -> one-pole coefficient 0.20..0.60.
        let comb_feedback = size * 0.28 + 0.70;
        let damp1 = damping * 0.40 + 0.20;
        Self {
            comb_feedback,
            damp1,
            damp2: 1.0 - damp1,
            mix: (mix_pct / 100.0).clamp(0.0, 1.0),
        }
    }
}

impl ReverbState {
    /// Process one stereo frame. Returns the (left, right) pair with the wet
    /// reverb added to the dry input. For mono input the same signal is fed to
    /// both channels and the caller uses the left output.
    pub fn process(&mut self, left: f32, right: f32, coefs: &ReverbCoefs) -> (f32, f32) {
        if self.combs_l.is_empty() {
            return (left, right);
        }
        let input = (left + right) * 0.5;

        let mut wet_l = 0.0f32;
        for c in self.combs_l.iter_mut() {
            let output = c.buffer[c.idx];
            c.store = output * coefs.damp1 + c.store * coefs.damp2;
            c.buffer[c.idx] = input + c.store * coefs.comb_feedback;
            c.idx = (c.idx + 1) % c.buffer.len();
            wet_l += output;
        }
        let mut wet_r = 0.0f32;
        for c in self.combs_r.iter_mut() {
            let output = c.buffer[c.idx];
            c.store = output * coefs.damp1 + c.store * coefs.damp2;
            c.buffer[c.idx] = input + c.store * coefs.comb_feedback;
            c.idx = (c.idx + 1) % c.buffer.len();
            wet_r += output;
        }

        let mut wet_l = wet_l * FIXED_GAIN;
        for a in self.allpasses_l.iter_mut() {
            let bufout = a.buffer[a.idx];
            a.buffer[a.idx] = wet_l + bufout * ALLPASS_FEEDBACK;
            wet_l = -wet_l + bufout;
            a.idx = (a.idx + 1) % a.buffer.len();
        }
        let mut wet_r = wet_r * FIXED_GAIN;
        for a in self.allpasses_r.iter_mut() {
            let bufout = a.buffer[a.idx];
            a.buffer[a.idx] = wet_r + bufout * ALLPASS_FEEDBACK;
            wet_r = -wet_r + bufout;
            a.idx = (a.idx + 1) % a.buffer.len();
        }

        (left + wet_l * coefs.mix, right + wet_r * coefs.mix)
    }
}
