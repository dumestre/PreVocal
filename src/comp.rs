//! Simple feedforward compressor (peak detector, one-pole attack/release).
//!
//! The envelope is smoothed in the linear domain with per-sample one-pole
//! filters whose time constant comes from the attack/release controls. Gain
//! reduction is computed in dB from the envelope overshoot above the
//! threshold, then applied as linear gain along with the makeup gain.
//! Per-channel state lives in [`CompressorState`]; block-invariant settings
//! (threshold, ratio, times, makeup) are baked into [`CompressorCoefs`].

use nice_plug::prelude::util;

/// Per-channel running state (the smoothed peak envelope).
#[derive(Clone, Copy, Default)]
pub struct CompressorState {
    envelope: f32,
}

/// Block-invariant compressor settings, pre-computed once per audio block.
#[derive(Clone, Copy)]
pub struct CompressorCoefs {
    attack: f32,
    release: f32,
    threshold_db: f32,
    gain_reduction_per_db: f32,
    makeup_gain: f32,
}

impl CompressorCoefs {
    pub fn new(
        attack_ms: f32,
        release_ms: f32,
        threshold_db: f32,
        ratio: f32,
        makeup_db: f32,
        sample_rate: f32,
    ) -> Self {
        Self {
            attack: (-1.0 / ((attack_ms / 1000.0) * sample_rate)).exp(),
            release: (-1.0 / ((release_ms / 1000.0) * sample_rate)).exp(),
            threshold_db,
            gain_reduction_per_db: -(1.0 - 1.0 / ratio.max(1.0)),
            makeup_gain: util::db_to_gain(makeup_db),
        }
    }
}

impl CompressorState {
    /// Process one sample: returns the compressed (and made-up) sample.
    pub fn process(&mut self, input: f32, coefs: &CompressorCoefs) -> f32 {
        let level = input.abs().max(1e-6);
        let coef = if level > self.envelope {
            coefs.attack
        } else {
            coefs.release
        };
        self.envelope = coef * self.envelope + (1.0 - coef) * level;

        let env_db = 20.0 * self.envelope.log10();
        let overshoot_db = (env_db - coefs.threshold_db).max(0.0);
        let gain_db = overshoot_db * coefs.gain_reduction_per_db;
        let gain = 10.0_f32.powf(gain_db / 20.0) * coefs.makeup_gain;
        input * gain
    }
}
