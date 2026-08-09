#![allow(non_snake_case)]

use nice_plug::prelude::*;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::sync::Once;
use std::panic;

// Immediate file log on plugin load (before any other init)
static INIT_LOG: Once = Once::new();
fn log_plugin_load() {
    INIT_LOG.call_once(|| {
        let _ = std::fs::write(
            r"C:\temp\prevocal_load.log",
            format!("[{}] PreVocal plugin loaded\n", chrono::Local::now().format("%H:%M:%S%.3f")),
        );
    });
}

// Panic hook to catch crashes
fn set_panic_hook() {
    panic::set_hook(Box::new(|info| {
        let _ = std::fs::write(
            r"C:\temp\prevocal_panic.log",
            format!("[{}] PANIC: {}\n", chrono::Local::now().format("%H:%M:%S%.3f"), info),
        );
    }));
}

mod comp;
mod delay;
mod editor;
mod presets;
mod reverb;

use comp::{CompressorCoefs, CompressorState};
use delay::{DelayCoefs, DelayState};
use reverb::{ReverbCoefs, ReverbState};
pub use presets::{preset_names, snapshot_preset, Preset, PRESETS};

// ---------------------------------------------------------------------
// Meter scale. Levels are exposed to the UI as a 0..1 mapping of the
// dBFS scale: -60 dB => 0.0, 0 dB => 1.0. The Slint meter colors the
// zones green (< -6 dB), yellow (-6..-3 dB) and red (>= -3 dB clip).
// ---------------------------------------------------------------------

/// Maps an RMS amplitude to the UI's 0..1 meter level (-60 dB..0 dB).
pub fn level_to_meter(rms: f32) -> f32 {
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Realtime meter state shared between the audio thread and the UI (plugin
/// editor or standalone). Written lock-free-ish (a short Mutex lock) from the
/// DSP, read by a UI poll loop.
#[derive(Default, Clone, Copy)]
pub struct MeterState {
    /// Smoothed input level on the UI's 0..1 scale.
    pub in_level: f32,
    /// Peak input amplitude (linear, 0..1).
    pub in_peak: f32,
    /// Smoothed output level on the UI's 0..1 scale.
    pub out_level: f32,
    /// Peak output amplitude (linear, 0..1).
    pub out_peak: f32,
}

pub struct PreVocal {
    dsp: PreVocalDsp,
}

#[derive(Params)]
pub struct PreVocalParams {
    #[id = "drive"]
    pub drive: FloatParam,

    #[id = "hpf"]
    pub hpf: FloatParam,

    #[id = "lpf"]
    pub lpf: FloatParam,

    #[id = "air"]
    pub air: FloatParam,

    #[id = "tube_character"]
    pub tube_character: FloatParam,

    #[id = "tube_sag"]
    pub tube_sag: FloatParam,

    #[id = "comp_bypass"]
    pub comp_bypass: BoolParam,

    #[id = "comp_thresh"]
    pub comp_thresh: FloatParam,

    #[id = "comp_ratio"]
    pub comp_ratio: FloatParam,

    #[id = "comp_attack"]
    pub comp_attack: FloatParam,

    #[id = "comp_release"]
    pub comp_release: FloatParam,

    #[id = "comp_makeup"]
    pub comp_makeup: FloatParam,

    #[id = "delay_bypass"]
    pub delay_bypass: BoolParam,

    #[id = "delay_time"]
    pub delay_time: FloatParam,

    #[id = "delay_feedback"]
    pub delay_feedback: FloatParam,

    #[id = "delay_mix"]
    pub delay_mix: FloatParam,

    #[id = "reverb_bypass"]
    pub reverb_bypass: BoolParam,

    #[id = "reverb_size"]
    pub reverb_size: FloatParam,

    #[id = "reverb_damping"]
    pub reverb_damping: FloatParam,

    #[id = "reverb_mix"]
    pub reverb_mix: FloatParam,

    #[id = "output_trim"]
    pub output_trim: FloatParam,
}

impl Default for PreVocal {
    fn default() -> Self {
        log_plugin_load();
        set_panic_hook();
        Self {
            dsp: PreVocalDsp::new(Arc::new(PreVocalParams::default())),
        }
    }
}

impl Default for PreVocalParams {
    fn default() -> Self {
        Self {
            drive: FloatParam::new(
                "Drive",
                util::db_to_gain(0.0),
                FloatRange::Skewed {
                    min: util::db_to_gain(0.0),
                    max: util::db_to_gain(24.0),
                    factor: FloatRange::gain_skew_factor(0.0, 24.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(50.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_gain_to_db(2))
            .with_string_to_value(formatters::s2v_f32_gain_to_db()),

            hpf: FloatParam::new(
                "HPF",
                20.0,
                FloatRange::Skewed {
                    min: 20.0,
                    max: 200.0,
                    factor: FloatRange::skew_factor(20.0),
                },
            )
            .with_smoother(SmoothingStyle::Linear(100.0))
            .with_unit(" Hz")
            .with_value_to_string(formatters::v2s_f32_rounded(0)),

            lpf: FloatParam::new(
                "LPF",
                20_000.0,
                FloatRange::Skewed {
                    min: 500.0,
                    max: 20_000.0,
                    factor: FloatRange::skew_factor(2000.0),
                },
            )
            .with_smoother(SmoothingStyle::Linear(100.0))
            .with_unit(" Hz")
            .with_value_to_string(formatters::v2s_f32_rounded(0)),

            air: FloatParam::new(
                "Air",
                0.0,
                FloatRange::Linear { min: 0.0, max: 6.0 },
            )
            .with_smoother(SmoothingStyle::Linear(100.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            tube_character: FloatParam::new(
                "Character",
                0.35,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_value_to_string(formatters::v2s_f32_rounded(2)),

            tube_sag: FloatParam::new(
                "Sag",
                0.0,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_value_to_string(formatters::v2s_f32_rounded(2)),

            comp_bypass: BoolParam::new("Compressor Bypass", false),

            comp_thresh: FloatParam::new(
                "Comp Threshold",
                -18.0,
                FloatRange::Skewed {
                    min: -60.0,
                    max: 0.0,
                    factor: FloatRange::skew_factor(-20.0),
                },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            comp_ratio: FloatParam::new(
                "Comp Ratio",
                3.0,
                FloatRange::Linear { min: 1.0, max: 20.0 },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            comp_attack: FloatParam::new(
                "Comp Attack",
                5.0,
                FloatRange::Skewed {
                    min: 0.1,
                    max: 100.0,
                    factor: FloatRange::skew_factor(5.0),
                },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            comp_release: FloatParam::new(
                "Comp Release",
                100.0,
                FloatRange::Skewed {
                    min: 10.0,
                    max: 1_000.0,
                    factor: FloatRange::skew_factor(100.0),
                },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            comp_makeup: FloatParam::new(
                "Comp Makeup",
                util::db_to_gain(0.0),
                FloatRange::Skewed {
                    min: util::db_to_gain(0.0),
                    max: util::db_to_gain(24.0),
                    factor: FloatRange::gain_skew_factor(0.0, 24.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(50.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_gain_to_db(2))
            .with_string_to_value(formatters::s2v_f32_gain_to_db()),

            delay_bypass: BoolParam::new("Delay Bypass", false),

            delay_time: FloatParam::new(
                "Delay Time",
                300.0,
                FloatRange::Skewed {
                    min: 1.0,
                    max: 1_000.0,
                    factor: FloatRange::skew_factor(300.0),
                },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            delay_feedback: FloatParam::new(
                "Delay Feedback",
                30.0,
                FloatRange::Skewed {
                    min: 0.0,
                    max: 90.0,
                    factor: FloatRange::skew_factor(30.0),
                },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_unit(" %")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            delay_mix: FloatParam::new(
                "Delay Mix",
                15.0,
                FloatRange::Skewed {
                    min: 0.0,
                    max: 100.0,
                    factor: FloatRange::skew_factor(15.0),
                },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_unit(" %")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            reverb_bypass: BoolParam::new("Reverb Bypass", true),

            reverb_size: FloatParam::new(
                "Reverb Size",
                0.5,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_value_to_string(formatters::v2s_f32_rounded(2)),

            reverb_damping: FloatParam::new(
                "Reverb Damping",
                0.5,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_value_to_string(formatters::v2s_f32_rounded(2)),

            reverb_mix: FloatParam::new(
                "Reverb Mix",
                15.0,
                FloatRange::Skewed {
                    min: 0.0,
                    max: 100.0,
                    factor: FloatRange::skew_factor(15.0),
                },
            )
            .with_smoother(SmoothingStyle::Linear(50.0))
            .with_unit(" %")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            output_trim: FloatParam::new(
                "Output Trim",
                util::db_to_gain(0.0),
                FloatRange::Skewed {
                    min: util::db_to_gain(-12.0),
                    max: util::db_to_gain(12.0),
                    factor: FloatRange::gain_skew_factor(-12.0, 12.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(50.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_gain_to_db(2))
            .with_string_to_value(formatters::s2v_f32_gain_to_db()),
        }
    }
}

impl Plugin for PreVocal {
    const NAME: &'static str = "PreVocal";
    const VENDOR: &'static str = "PreVocal";
    const URL: &'static str = "https://example.com/prevocal";
    const EMAIL: &'static str = "support@prevocal.example";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.dsp.params()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        Some(Box::new(editor::SlintEditor::new(
            self.dsp.params(),
            self.dsp.meters(),
        )))
    }

    fn initialize(
        &mut self,
        audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.dsp.set_sample_rate(buffer_config.sample_rate);
        self.dsp
            .resize(audio_io_layout.main_input_channels.map_or(2, |c| c.get() as usize));
        tracing::info!(
            "PreVocal: initialize() sample_rate={} buffer_size={} channels_in={:?} channels_out={:?}",
            buffer_config.sample_rate,
            buffer_config.max_buffer_size,
            audio_io_layout.main_input_channels.map(|c| c.get()),
            audio_io_layout.main_output_channels.map(|c| c.get()),
        );
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Throttled diagnostics: confirms the host actually runs the DSP and
        // shows the block size/channel count/input+output level (Cubase vs FL
        // Studio). Output is logged after `process_block` so a silent output
        // with a live input points at either the DSP or the saved param state.
        let do_log = self.dsp.last_process_log.elapsed().as_secs() >= 1;
        let (mut in_sum_sq, mut in_peak) = (0.0f32, 0.0f32);
        if do_log {
            for channel in buffer.as_slice().iter() {
                for &s in channel.iter() {
                    in_sum_sq += s * s;
                    in_peak = in_peak.max(s.abs());
                }
            }
        }
        self.dsp.process_block(buffer.as_slice());
        if do_log {
            self.dsp.last_process_log = std::time::Instant::now();
            let channels = buffer.as_slice();
            let num_frames = channels.first().map_or(0, |c| c.len());
            let mut out_sum_sq = 0.0f32;
            let mut out_peak = 0.0f32;
            for channel in channels.iter() {
                for &s in channel.iter() {
                    out_sum_sq += s * s;
                    out_peak = out_peak.max(s.abs());
                }
            }
            let count = (num_frames * channels.len()) as f32;
            let rms = |sum_sq: f32| if num_frames == 0 || count == 0.0 {
                0.0
            } else {
                (sum_sq / count).sqrt()
            };
            let params = self.dsp.params();
            tracing::info!(
                "PreVocal: process() channels={} frames={} in_rms={:.6} in_peak={:.6} out_rms={:.6} out_peak={:.6} drive_db={:.2} trim_db={:.2} hpf={:.1} lpf={:.1} comp_bypass={} delay_bypass={} reverb_bypass={}",
                channels.len(),
                num_frames,
                rms(in_sum_sq),
                in_peak,
                rms(out_sum_sq),
                out_peak,
                util::gain_to_db(params.drive.modulated_plain_value()),
                util::gain_to_db(params.output_trim.modulated_plain_value()),
                params.hpf.modulated_plain_value(),
                params.lpf.modulated_plain_value(),
                params.comp_bypass.value(),
                params.delay_bypass.value(),
                params.reverb_bypass.value(),
            );
        }
        ProcessStatus::Normal
    }

    fn deactivate(&mut self) {
        tracing::info!("PreVocal: deactivate()");
    }

    fn setup_logger() -> Option<bool> {
        Some(
            tracing::subscriber::set_global_default(
                tracing_subscriber::FmtSubscriber::builder()
                    .with_max_level(tracing::level_filters::LevelFilter::TRACE)
                    .with_thread_names(true)
                    .with_ansi(false)
                    .with_writer(move || {
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(r"C:\temp\prevocal_plugin.log")
                            .unwrap_or_else(|_| std::fs::File::create(r"C:\temp\prevocal_plugin.log").unwrap())
                    })
                    .finish(),
            )
            .is_ok(),
        )
    }

    fn exit_dll() {
        editor::shutdown_editor_thread();
    }
}

impl ClapPlugin for PreVocal {
    const CLAP_ID: &'static str = "com.prevocal.prevocal";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("Vocal Bus Preamp with soft saturation, HPF/LPF, air boost, compressor, stereo delay and reverb.");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Stereo,
        ClapFeature::Mono,
        ClapFeature::Utility,
    ];
}

impl Vst3Plugin for PreVocal {
    const VST3_CLASS_ID: [u8; 16] = *b"PreVocalPreVocal";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Fx,
        Vst3SubCategory::Stereo,
        Vst3SubCategory::Mono,
        Vst3SubCategory::Tools,
    ];
}

/// Biquad filter coefficients (Direct Form I, normalized).
#[derive(Clone, Copy)]
pub struct BiquadCoeffs {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

/// Running state of a biquad filter (previous inputs/outputs).
#[derive(Clone, Copy, Default)]
pub struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BiquadState {
    fn process(&mut self, input: f32, coeffs: &BiquadCoeffs) -> f32 {
        let output = coeffs.b0 * input
            + coeffs.b1 * self.x1
            + coeffs.b2 * self.x2
            - coeffs.a1 * self.y1
            - coeffs.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = input;
        self.y2 = self.y1;
        self.y1 = output;
        output
    }
}

/// One-pole lowpass filter state used inside the tube stage.
#[derive(Clone, Copy, Default)]
pub struct OnePoleLp {
    y: f32,
}

impl OnePoleLp {
    fn process(&mut self, input: f32, coeff: f32) -> f32 {
        self.y += coeff * (input - self.y);
        self.y
    }
}

/// One-pole (DC-blocking) highpass filter state used at the tube input to
/// remove the DC component introduced by the bias. Implemented as the
/// difference `x - lp` so only the DC/low band is removed and the audio
/// band passes with unity gain.
#[derive(Clone, Copy, Default)]
pub struct OnePoleHp {
    lp: OnePoleLp,
}

impl OnePoleHp {
    fn process(&mut self, input: f32, coeff: f32) -> f32 {
        input - self.lp.process(input, coeff)
    }
}

/// Per-channel tube/valve stage state: input coupling HPF, drive-dependent
/// input LPF, output transformer LPF and the sag envelope follower.
#[derive(Clone, Copy, Default)]
pub struct TubeState {
    pub coupling_hp: OnePoleHp,
    pub input_lp: OnePoleLp,
    pub transformer_lp: OnePoleLp,
    pub envelope: f32,
}

/// Fixed per-block coefficients for the tube stage.
#[derive(Clone, Copy)]
pub struct TubeCoefs {
    /// Asymmetry bias (even-harmonic "warmth"). 0 => symmetric tanh.
    pub bias: f32,
    /// Input one-pole LPF coefficient (darkens as drive increases).
    pub input_lp_coeff: f32,
    /// Input coupling one-pole HPF coefficient (~45 Hz).
    pub coupling_hp_coeff: f32,
    /// Output transformer one-pole LPF coefficient (~5 kHz).
    pub transformer_lp_coeff: f32,
    /// Sag strength: `g_eff = 1 / (1 + sag_k * envelope)`.
    pub sag_k: f32,
    /// One-pole release coefficient for the sag envelope (~80 ms).
    pub sag_release_coeff: f32,
}

/// Per-channel filter state grouping the tube stage, the HPF, LPF, Air
/// high-shelf biquads and the compressor envelope.
#[derive(Clone, Copy, Default)]
pub struct ChannelFilter {
    pub tube: TubeState,
    pub hpf: BiquadState,
    pub lpf: BiquadState,
    pub air: BiquadState,
    pub comp: CompressorState,
}

/// Coefficients for a 2nd order Butterworth high-pass (12 dB/oct), derived from an
/// RBJ audio EQ cookbook bilinear transform.
pub fn butterworth_2p_highpass_coeffs(freq: f32, sample_rate: f32) -> BiquadCoeffs {
    let omega = 2.0 * std::f32::consts::PI * freq / sample_rate;
    let alpha = (omega / 2.0).sin() / (2.0_f32).sqrt();
    let cos_omega = omega.cos();

    let b0 = (1.0 + cos_omega) / 2.0;
    let b1 = -(1.0 + cos_omega);
    let b2 = (1.0 + cos_omega) / 2.0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cos_omega;
    let a2 = 1.0 - alpha;

    BiquadCoeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

/// Coefficients for a 2nd order Butterworth low-pass (12 dB/oct), derived from an
/// RBJ audio EQ cookbook bilinear transform.
pub fn butterworth_2p_lowpass_coeffs(freq: f32, sample_rate: f32) -> BiquadCoeffs {
    let omega = 2.0 * std::f32::consts::PI * freq / sample_rate;
    let alpha = (omega / 2.0).sin() / (2.0_f32).sqrt();
    let cos_omega = omega.cos();

    let b0 = (1.0 - cos_omega) / 2.0;
    let b1 = 1.0 - cos_omega;
    let b2 = (1.0 - cos_omega) / 2.0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cos_omega;
    let a2 = 1.0 - alpha;

    BiquadCoeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

/// Coefficients for a 2nd-order high-shelf filter (12 dB/octave) using the
/// RBJ audio EQ cookbook formulas. `db_gain` is the boost/cut in dB at and
/// above the corner frequency `freq`. A Butterworth-style Q (1/√2) is used.
pub fn highshelf_2p_coeffs(freq: f32, db_gain: f32, sample_rate: f32) -> BiquadCoeffs {
    let a = 10.0_f32.powf(db_gain / 40.0);
    let omega = 2.0 * std::f32::consts::PI * freq / sample_rate;
    let sin_omega = omega.sin();
    let cos_omega = omega.cos();
    let alpha = sin_omega / (2.0 * 2.0_f32.sqrt());
    let sqrt_a = a.sqrt();

    let b0 = a * ((a + 1.0) - (a - 1.0) * cos_omega + 2.0 * sqrt_a * alpha);
    let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_omega);
    let b2 = a * ((a + 1.0) - (a - 1.0) * cos_omega - 2.0 * sqrt_a * alpha);
    let a0 = (a + 1.0) + (a - 1.0) * cos_omega + 2.0 * sqrt_a * alpha;
    let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_omega);
    let a2 = (a + 1.0) - (a - 1.0) * cos_omega - 2.0 * sqrt_a * alpha;

    BiquadCoeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

/// Compute the fixed-per-block tube stage coefficients.
pub fn tube_coefs(tube_character: f32, tube_sag: f32, drive: f32, sample_rate: f32) -> TubeCoefs {
    // Input LPF corner drops from ~6 kHz (clean) to ~1.2 kHz (fully driven),
    // emulating a preamp grid stage that darkens as it saturates.
    let drive_db = (20.0 * drive.log10().max(0.0)).clamp(0.0, 24.0);
    let drive_norm = drive_db / 24.0;
    let input_lp_freq = 6000.0 * 0.2_f32.powf(drive_norm);
    let one_pole_lp = |freq: f32| 1.0 - (-2.0 * std::f32::consts::PI * freq / sample_rate).exp();
    TubeCoefs {
        bias: 0.15 * tube_character,
        input_lp_coeff: one_pole_lp(input_lp_freq),
        coupling_hp_coeff: one_pole_lp(45.0),
        transformer_lp_coeff: one_pole_lp(5000.0),
        sag_k: tube_sag * 0.3,
        sag_release_coeff: (-1.0 / (sample_rate * 0.08)).exp(),
    }
}

/// Process one sample through the tube/valve stage:
///
/// `coupling HPF -> drive-dependent input LPF -> sag -> asymmetric tanh -> transformer LPF`
///
/// `input` is expected to be the already drive-scaled signal (`input * drive`).
/// The asymmetric saturation (`tanh(g*(x+b)) - tanh(g*b)`) generates even
/// harmonics (2nd = warmth). The `-tanh(g*b)` removes the DC of the bias and
/// the normalization keeps the small-signal gain at 1 (so at `character=0`
/// the stage is exactly the previous symmetric `tanh`).
#[inline]
pub fn tube_stage(input: f32, coefs: &TubeCoefs, state: &mut TubeState) -> f32 {
    let x = state.coupling_hp.process(input, coefs.coupling_hp_coeff);
    let x = state.input_lp.process(x, coefs.input_lp_coeff);

    // Sag: peak detector with instant attack / ~80 ms release reduces the
    // effective drive under load (power-amp "sag" feel).
    let abs = x.abs();
    state.envelope = if abs > state.envelope {
        abs
    } else {
        state.envelope * coefs.sag_release_coeff + (1.0 - coefs.sag_release_coeff) * abs
    };
    let g = 1.0 / (1.0 + coefs.sag_k * state.envelope);

    let gb = g * coefs.bias;
    let y = (g * (x + coefs.bias)).tanh() - gb.tanh();
    let y = y / (1.0 - gb * gb);
    state.transformer_lp.process(y, coefs.transformer_lp_coeff)
}

/// Process a single sample through the complete PreVocal chain:
///
/// `input -> Drive -> Tube stage -> HPF -> LPF -> Air (high-shelf) -> Compressor -> Output Trim`
///
/// When `comp_bypass` is set the compressor's envelope keeps tracking the
/// signal (so re-enabling it doesn't pump), but no gain is applied.
#[allow(clippy::too_many_arguments)]
pub fn process_sample(
    input: f32,
    drive: f32,
    trim: f32,
    tube: &TubeCoefs,
    tube_state: &mut TubeState,
    hpf: &BiquadCoeffs,
    hpf_state: &mut BiquadState,
    lpf: &BiquadCoeffs,
    lpf_state: &mut BiquadState,
    air: &BiquadCoeffs,
    air_state: &mut BiquadState,
    comp: &CompressorCoefs,
    comp_state: &mut CompressorState,
    comp_bypass: bool,
) -> f32 {
    let x = tube_stage(input * drive, tube, tube_state);
    let y = hpf_state.process(x, hpf);
    let w = lpf_state.process(y, lpf);
    let z = air_state.process(w, air);
    let c = if comp_bypass {
        comp_state.process(z, comp);
        z
    } else {
        comp_state.process(z, comp)
    };
    c * trim
}

/// Shared realtime DSP engine. Used by both the plugin and the standalone binary so the
/// audio code stays in a single place.
pub struct PreVocalDsp {
    params: Arc<PreVocalParams>,
    sample_rate: f32,
    filter_states: Vec<ChannelFilter>,
    delay: DelayState,
    reverb: ReverbState,
    /// Realtime IN/OUT meter levels, shared with the UI (editor / standalone).
    meters: Arc<Mutex<MeterState>>,
    /// Last time we logged audio-path diagnostics (throttled to ~1 Hz).
    last_process_log: std::time::Instant,
}

/// One set of smoothed parameter values shared by a whole block of audio.
#[derive(Clone, Copy)]
struct BlockParams {
    drive: f32,
    hpf_freq: f32,
    lpf_freq: f32,
    air_db: f32,
    tube_character: f32,
    tube_sag: f32,
    comp_thresh_db: f32,
    comp_ratio: f32,
    comp_attack_ms: f32,
    comp_release_ms: f32,
    comp_makeup_db: f32,
    comp_bypass: bool,
    delay_time_ms: f32,
    delay_feedback_pct: f32,
    delay_mix_pct: f32,
    delay_bypass: bool,
    reverb_size: f32,
    reverb_damping: f32,
    reverb_mix_pct: f32,
    reverb_bypass: bool,
    trim: f32,
}

impl PreVocalDsp {
    pub fn new(params: Arc<PreVocalParams>) -> Self {
        Self {
            params,
            sample_rate: 48_000.0,
            filter_states: Vec::new(),
            delay: DelayState::default(),
            reverb: ReverbState::default(),
            meters: Arc::new(Mutex::new(MeterState::default())),
            last_process_log: std::time::Instant::now(),
        }
    }

    pub fn params(&self) -> Arc<PreVocalParams> {
        self.params.clone()
    }

    /// Handle to the shared realtime meter state (audio thread writes, UI reads).
    pub fn meters(&self) -> Arc<Mutex<MeterState>> {
        self.meters.clone()
    }

    /// Set the sample rate and resync every smoother so the UI and automation always start
    /// from the current parameter values.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.delay.set_sample_rate(sample_rate);
        self.reverb.set_sample_rate(sample_rate);
        for (_, param_ptr, _) in self.params.param_map() {
            unsafe { param_ptr._internal_update_smoother(sample_rate, true) };
        }
    }

    /// Allocate filter state for each audio channel.
    pub fn resize(&mut self, num_channels: usize) {
        self.filter_states.resize(num_channels, ChannelFilter::default());
    }

    /// Advance the smoothers by a whole block and return the parameter values to use.
    /// Returns `None` for an empty block.
    fn block_params(&self, num_frames: usize) -> Option<BlockParams> {
        if num_frames == 0 {
            return None;
        }
        let steps = num_frames as u32;
        Some(BlockParams {
            drive: self.params.drive.smoothed.next_step(steps),
            hpf_freq: self.params.hpf.smoothed.next_step(steps),
            lpf_freq: self.params.lpf.smoothed.next_step(steps),
            air_db: self.params.air.smoothed.next_step(steps),
            tube_character: self.params.tube_character.smoothed.next_step(steps),
            tube_sag: self.params.tube_sag.smoothed.next_step(steps),
            comp_thresh_db: self.params.comp_thresh.smoothed.next_step(steps),
            comp_ratio: self.params.comp_ratio.smoothed.next_step(steps),
            comp_attack_ms: self.params.comp_attack.smoothed.next_step(steps),
            comp_release_ms: self.params.comp_release.smoothed.next_step(steps),
            comp_makeup_db: util::gain_to_db(self.params.comp_makeup.smoothed.next_step(steps)),
            comp_bypass: self.params.comp_bypass.value(),
            delay_time_ms: self.params.delay_time.smoothed.next_step(steps),
            delay_feedback_pct: self.params.delay_feedback.smoothed.next_step(steps),
            delay_mix_pct: self.params.delay_mix.smoothed.next_step(steps),
            delay_bypass: self.params.delay_bypass.value(),
            reverb_size: self.params.reverb_size.smoothed.next_step(steps),
            reverb_damping: self.params.reverb_damping.smoothed.next_step(steps),
            reverb_mix_pct: self.params.reverb_mix.smoothed.next_step(steps),
            reverb_bypass: self.params.reverb_bypass.value(),
            trim: self.params.output_trim.smoothed.next_step(steps),
        })
    }

    fn coeffs_for(&self, p: &BlockParams) -> (BiquadCoeffs, BiquadCoeffs, BiquadCoeffs) {
        (
            butterworth_2p_highpass_coeffs(p.hpf_freq, self.sample_rate),
            butterworth_2p_lowpass_coeffs(p.lpf_freq, self.sample_rate),
            highshelf_2p_coeffs(10_000.0, p.air_db, self.sample_rate),
        )
    }

    fn comp_coefs(&self, p: &BlockParams) -> CompressorCoefs {
        CompressorCoefs::new(
            p.comp_attack_ms,
            p.comp_release_ms,
            p.comp_thresh_db,
            p.comp_ratio,
            p.comp_makeup_db,
            self.sample_rate,
        )
    }

    fn tube_coefs(&self, p: &BlockParams) -> TubeCoefs {
        tube_coefs(p.tube_character, p.tube_sag, p.drive, self.sample_rate)
    }

    fn delay_coefs(&self, p: &BlockParams) -> DelayCoefs {
        DelayCoefs::new(
            p.delay_time_ms,
            p.delay_feedback_pct,
            p.delay_mix_pct,
            self.sample_rate,
        )
    }

    fn reverb_coefs(&self, p: &BlockParams) -> ReverbCoefs {
        ReverbCoefs::new(p.reverb_size, p.reverb_damping, p.reverb_mix_pct)
    }

    /// Process one block of audio across all channels. Parameters are read from the shared
    /// [`Arc<PreVocalParams>`] once per block, so the smoothers advance in a consistent way.
    pub fn process_block(&mut self, channels: &mut [&mut [f32]]) {
        let num_frames = channels.first().map_or(0, |c| c.len());
        let Some(p) = self.block_params(num_frames) else {
            return;
        };
        let (hpf_coeffs, lpf_coeffs, air_coeffs) = self.coeffs_for(&p);
        let comp_coefs = self.comp_coefs(&p);
        let tube_coefs = self.tube_coefs(&p);
        let delay_coefs = self.delay_coefs(&p);
        let reverb_coefs = self.reverb_coefs(&p);
        let stereo = channels.len() > 1;

        // IN meter: RMS + peak of the raw, pre-DSP input.
        let mut in_sum_sq = 0.0f32;
        let mut in_peak = 0.0f32;
        for channel in channels.iter() {
            for &s in channel.iter() {
                in_sum_sq += s * s;
                let a = s.abs();
                if a > in_peak {
                    in_peak = a;
                }
            }
        }
        let in_rms = if num_frames == 0 {
            0.0
        } else {
            (in_sum_sq / (num_frames * channels.len()) as f32).sqrt()
        };

        for (channel, samples) in channels.iter_mut().enumerate() {
            let mut tube = self.filter_states[channel].tube;
            let mut hpf = self.filter_states[channel].hpf;
            let mut lpf = self.filter_states[channel].lpf;
            let mut air = self.filter_states[channel].air;
            let mut comp = self.filter_states[channel].comp;
            for sample in samples.iter_mut() {
                *sample = process_sample(
                    *sample,
                    p.drive,
                    p.trim,
                    &tube_coefs,
                    &mut tube,
                    &hpf_coeffs,
                    &mut hpf,
                    &lpf_coeffs,
                    &mut lpf,
                    &air_coeffs,
                    &mut air,
                    &comp_coefs,
                    &mut comp,
                    p.comp_bypass,
                );
            }
            self.filter_states[channel] = ChannelFilter { tube, hpf, lpf, air, comp };
        }

        // Stereo delay at the end of the chain: the principal signal stays mono,
        // only the echo taps are stereo.
        if !p.delay_bypass {
            #[allow(clippy::needless_range_loop)]
            for frame in 0..num_frames {
                let left = channels[0][frame];
                let right = if stereo { channels[1][frame] } else { left };
                let (out_l, out_r) = self.delay.process(left, right, &delay_coefs, stereo);
                channels[0][frame] = out_l;
                if stereo {
                    channels[1][frame] = out_r;
                }
            }
        }

        // Reverb last in the chain: the delay echoes (and the dry signal) hit
        // the reverb tail, keeping the decay natural instead of echoing it.
        if !p.reverb_bypass {
            #[allow(clippy::needless_range_loop)]
            for frame in 0..num_frames {
                let left = channels[0][frame];
                let right = if stereo { channels[1][frame] } else { left };
                let (out_l, out_r) = self.reverb.process(left, right, &reverb_coefs);
                channels[0][frame] = out_l;
                if stereo {
                    channels[1][frame] = out_r;
                }
            }
        }
    
        // OUT meter: RMS + peak of the fully processed signal.
        let mut out_sum_sq = 0.0f32;
        let mut out_peak = 0.0f32;
        for channel in channels.iter() {
            for &s in channel.iter() {
                out_sum_sq += s * s;
                let a = s.abs();
                if a > out_peak {
                    out_peak = a;
                }
            }
        }
        let out_rms = if num_frames == 0 {
            0.0
        } else {
            (out_sum_sq / (num_frames * channels.len()) as f32).sqrt()
        };

        if let Ok(mut m) = self.meters.lock() {
            m.in_level = level_to_meter(in_rms);
            m.in_peak = in_peak;
            m.out_level = level_to_meter(out_rms);
            m.out_peak = out_peak;
        }
    }

    /// Process one block of interleaved audio in place (frame-major: `[ch0, ch1, ch0, ch1, ...]`).
    /// Allocates nothing; used by the standalone for low-latency, glitch-free callbacks.
    pub fn process_interleaved(&mut self, samples: &mut [f32], num_channels: usize) {
        let num_frames = samples.len() / num_channels;
        let Some(p) = self.block_params(num_frames) else {
            return;
        };
        let (hpf_coeffs, lpf_coeffs, air_coeffs) = self.coeffs_for(&p);
        let comp_coefs = self.comp_coefs(&p);
        let tube_coefs = self.tube_coefs(&p);
        let delay_coefs = self.delay_coefs(&p);
        let reverb_coefs = self.reverb_coefs(&p);
        let stereo = num_channels > 1;

        for frame in 0..num_frames {
            for channel in 0..num_channels {
                let idx = frame * num_channels + channel;
                let mut tube = self.filter_states[channel].tube;
                let mut hpf = self.filter_states[channel].hpf;
                let mut lpf = self.filter_states[channel].lpf;
                let mut air = self.filter_states[channel].air;
                let mut comp = self.filter_states[channel].comp;
                samples[idx] = process_sample(
                    samples[idx],
                    p.drive,
                    p.trim,
                    &tube_coefs,
                    &mut tube,
                    &hpf_coeffs,
                    &mut hpf,
                    &lpf_coeffs,
                    &mut lpf,
                    &air_coeffs,
                    &mut air,
                    &comp_coefs,
                    &mut comp,
                    p.comp_bypass,
                );
                self.filter_states[channel] = ChannelFilter { tube, hpf, lpf, air, comp };
            }
            // Stereo delay at the end of the chain (mono principal, stereo taps).
            if !p.delay_bypass {
                let idx = frame * num_channels;
                let left = samples[idx];
                let right = if stereo { samples[idx + 1] } else { left };
                let (out_l, out_r) = self.delay.process(left, right, &delay_coefs, stereo);
                samples[idx] = out_l;
                if stereo {
                    samples[idx + 1] = out_r;
                }
            }
            // Reverb after the delay, last in the chain.
            if !p.reverb_bypass {
                let idx = frame * num_channels;
                let left = samples[idx];
                let right = if stereo { samples[idx + 1] } else { left };
                let (out_l, out_r) = self.reverb.process(left, right, &reverb_coefs);
                samples[idx] = out_l;
                if stereo {
                    samples[idx + 1] = out_r;
                }
            }
        }
    }
}

nice_export_clap!(PreVocal);
nice_export_vst3!(PreVocal);

#[cfg(test)]
mod tests {
    use super::*;

    /// The tube stage must produce even harmonics (2nd = warmth) when
    /// `character > 0` and none when `character = 0`. Measured with a
    /// phase-independent (complex) 2nd-harmonic correlation after the
    /// filters settle.
    #[test]
    fn tube_curve_asymmetry() {
        let sr = 48_000.0;
        let drive = util::db_to_gain(12.0);
        let warm = tube_coefs(1.0, 0.0, drive, sr);
        let neutral = tube_coefs(0.0, 0.0, drive, sr);

        let h2_mag = |coefs: &TubeCoefs| -> f32 {
            let omega = 2.0 * std::f32::consts::PI * 1000.0 / sr;
            let n = 8000;
            let mut s = TubeState::default();
            for i in 0..6000 {
                let x = (omega * i as f32).sin() * 0.5;
                let _ = tube_stage(x, coefs, &mut s);
            }
            let mut re = 0.0f32;
            let mut im = 0.0f32;
            for i in 6000..n {
                let t = i as f32;
                let x = (omega * t).sin() * 0.5;
                let y = tube_stage(x, coefs, &mut s);
                let p = 2.0 * omega * t;
                re += y * p.cos();
                im += y * p.sin();
            }
            let m = (n - 6000) as f32;
            let re = 2.0 * re / m;
            let im = 2.0 * im / m;
            (re * re + im * im).sqrt()
        };

        assert!(h2_mag(&neutral) < 0.005, "neutral h2={}", h2_mag(&neutral));
        assert!(h2_mag(&warm) > 0.005, "warm h2={}", h2_mag(&warm));
    }

    /// The warmth bias must not change the overall level: the small-signal
    /// gain of the stage stays ~1 for any `character` value.
    #[test]
    fn tube_character_keeps_level() {
        let sr = 48_000.0;
        let drive = 1.0f32;
        let n = 4800;
        let omega = 2.0 * std::f32::consts::PI * 1000.0 / sr;
        let rms = |character: f32| {
            let coefs = tube_coefs(character, 0.0, drive, sr);
            let mut s = TubeState::default();
            let mut acc = 0.0f32;
            for i in 0..n {
                let x = (omega * i as f32).sin() * 0.01;
                let y = tube_stage(x, &coefs, &mut s);
                acc += y * y;
            }
            (acc / n as f32).sqrt()
        };
        let neutral = rms(0.0);
        let warm = rms(1.0);
        assert!(
            (warm / neutral - 1.0).abs() < 0.05,
            "neutral={neutral} warm={warm}"
        );
    }
}
