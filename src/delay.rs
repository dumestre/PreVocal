//! Stereo delay with a mono "center" signal.
//!
//! The dry signal is collapsed to mono (sum of the input channels) and
//! output identically on both channels. The echo taps are stereo: the left
//! tap runs at the selected delay time and the right tap at 1.5x that time,
//! creating a ping-pong-like widening effect while the principal signal stays
//! perfectly centered. Feedback is shared and feeds a single circular buffer.

const MAX_DELAY_SECONDS: f32 = 1.0;

/// Block-invariant delay settings, pre-computed once per audio block.
#[derive(Clone, Copy)]
pub struct DelayCoefs {
    delay_l_samples: usize,
    delay_r_samples: usize,
    feedback: f32,
    mix: f32,
}

/// Shared circular buffer plus write position.
#[derive(Default)]
pub struct DelayState {
    buffer: Vec<f32>,
    write_idx: usize,
    sample_rate: f32,
}

impl DelayState {
    /// (Re)allocate the circular buffer when the sample rate changes.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if (sample_rate - self.sample_rate).abs() > 0.5 {
            self.sample_rate = sample_rate;
            self.buffer = vec![0.0; (sample_rate * MAX_DELAY_SECONDS) as usize + 2];
            self.write_idx = 0;
        }
    }
}

impl DelayCoefs {
    pub fn new(time_ms: f32, feedback_pct: f32, mix_pct: f32, sample_rate: f32) -> Self {
        let max_len = (sample_rate * MAX_DELAY_SECONDS) as usize + 1;
        let delay_l_samples = ((time_ms / 1000.0) * sample_rate).round() as usize;
        let delay_l_samples = delay_l_samples.clamp(1, max_len);
        let delay_r_samples = ((delay_l_samples as f32) * 1.5).round() as usize;
        Self {
            delay_l_samples,
            delay_r_samples: delay_r_samples.clamp(1, max_len),
            feedback: (feedback_pct / 100.0).clamp(0.0, 0.9),
            // Square-root mix curve (same reasoning as the reverb): the echo
            // stays audible at low mix values and grows gradually to 100%.
            mix: (mix_pct / 100.0).clamp(0.0, 1.0).sqrt(),
        }
    }
}

impl DelayState {
    /// Process one stereo frame. Returns the processed (left, right) pair.
    pub fn process(&mut self, left: f32, right: f32, coefs: &DelayCoefs, stereo: bool) -> (f32, f32) {
        if self.buffer.is_empty() {
            return (left, right);
        }
        let len = self.buffer.len();
        // Mono "center" principal signal feeds the delay line.
        let dry = (left + right) * 0.5;

        let read_l = (self.write_idx + len - coefs.delay_l_samples) % len;
        let read_r = (self.write_idx + len - coefs.delay_r_samples) % len;
        let wet_l = self.buffer[read_l];
        let wet_r = self.buffer[read_r];

        self.buffer[self.write_idx] = dry + coefs.feedback * 0.5 * (wet_l + wet_r);
        self.write_idx = (self.write_idx + 1) % len;

        if stereo {
            (
                dry + wet_l * coefs.mix,
                dry + wet_r * coefs.mix,
            )
        } else {
            let wet = 0.5 * (wet_l + wet_r) * coefs.mix;
            (dry + wet, dry + wet)
        }
    }
}
