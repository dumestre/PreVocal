use nice_plug::prelude::*;
use std::num::NonZeroU32;
use std::sync::Arc;

pub struct PreVocal {
    dsp: PreVocalDsp,
}

#[derive(Params)]
pub struct PreVocalParams {
    #[id = "drive"]
    pub drive: FloatParam,

    #[id = "hpf"]
    pub hpf: FloatParam,

    #[id = "air"]
    pub air: FloatParam,

    #[id = "phase_flip"]
    pub phase_flip: BoolParam,

    #[id = "output_trim"]
    pub output_trim: FloatParam,
}

impl Default for PreVocal {
    fn default() -> Self {
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

            air: FloatParam::new(
                "Air",
                0.0,
                FloatRange::Linear { min: 0.0, max: 6.0 },
            )
            .with_smoother(SmoothingStyle::Linear(100.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_rounded(1)),

            phase_flip: BoolParam::new("Phase Flip", false),

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

    fn initialize(
        &mut self,
        audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.dsp.set_sample_rate(buffer_config.sample_rate);
        self.dsp
            .resize(audio_io_layout.main_input_channels.map_or(2, |c| c.get() as usize));
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        self.dsp.process_block(buffer.as_slice());
        ProcessStatus::Normal
    }

    fn deactivate(&mut self) {}

    fn setup_logger() -> Option<bool> {
        Some(
            tracing::subscriber::set_global_default(
                tracing_subscriber::FmtSubscriber::builder()
                    .with_max_level(if cfg!(debug_assertions) {
                        tracing::level_filters::LevelFilter::DEBUG
                    } else {
                        tracing::level_filters::LevelFilter::INFO
                    })
                    .with_ansi(false)
                    .with_writer(nice_plug::log::writer_from_env())
                    .finish(),
            )
            .is_ok(),
        )
    }
}

impl ClapPlugin for PreVocal {
    const CLAP_ID: &'static str = "com.prevocal.prevocal";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("Vocal Bus Preamp with soft saturation, HPF and air boost.");
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

/// Process a single sample through the complete PreVocal chain:
///
/// `input -> Drive (tanh) -> Air (high shelf gain) -> HPF -> Output Trim -> Phase Flip`
pub fn process_sample(
    input: f32,
    drive: f32,
    air_gain: f32,
    phase_invert: bool,
    trim: f32,
    hpf: &BiquadCoeffs,
    state: &mut BiquadState,
) -> f32 {
    let mut x = input * drive;
    x = x.tanh();
    x *= air_gain;
    let y = state.process(x, hpf);
    let mut out = y * trim;
    if phase_invert {
        out = -out;
    }
    out
}

/// Shared realtime DSP engine. Used by both the plugin and the standalone binary so the
/// audio code stays in a single place.
pub struct PreVocalDsp {
    params: Arc<PreVocalParams>,
    sample_rate: f32,
    filter_states: Vec<BiquadState>,
}

impl PreVocalDsp {
    pub fn new(params: Arc<PreVocalParams>) -> Self {
        Self {
            params,
            sample_rate: 48_000.0,
            filter_states: Vec::new(),
        }
    }

    pub fn params(&self) -> Arc<PreVocalParams> {
        self.params.clone()
    }

    /// Set the sample rate and resync every smoother so the UI and automation always start
    /// from the current parameter values.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        for (_, param_ptr, _) in self.params.param_map() {
            unsafe { param_ptr._internal_update_smoother(sample_rate, true) };
        }
    }

    /// Allocate filter state for each audio channel.
    pub fn resize(&mut self, num_channels: usize) {
        self.filter_states.resize(num_channels, BiquadState::default());
    }

    /// Process one block of audio across all channels. Parameters are read from the shared
    /// [`Arc<PreVocalParams>`] once per block, so the smoothers advance in a consistent way.
    pub fn process_block(&mut self, channels: &mut [&mut [f32]]) {
        let drive = self.params.drive.smoothed.next();
        let hpf_freq = self.params.hpf.smoothed.next();
        let air_gain = util::db_to_gain(self.params.air.smoothed.next());
        let phase_invert = self.params.phase_flip.modulated_plain_value();
        let trim = self.params.output_trim.smoothed.next();
        let hpf_coeffs = butterworth_2p_highpass_coeffs(hpf_freq, self.sample_rate);

        for (channel, samples) in channels.iter_mut().enumerate() {
            let mut state = self.filter_states[channel];
            for sample in samples.iter_mut() {
                *sample = process_sample(*sample, drive, air_gain, phase_invert, trim, &hpf_coeffs, &mut state);
            }
            self.filter_states[channel] = state;
        }
    }
}

nice_export_clap!(PreVocal);
nice_export_vst3!(PreVocal);
