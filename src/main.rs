//! PreVocal standalone binary.
//!
//! This target runs the same DSP as the plugin as a native application using Slint for
//! the GUI and cpal for realtime audio I/O, so it can be tested without a DAW on both
//! Windows and Linux. The DSP lives in `prevocal::PreVocalDsp` and is shared with the
//! plugin `lib.rs`.
//!
//! The audio graph is:
//!
//! ```text
//! system mic (default input device) -> DSP -> ring buffer -> speakers (default output device)
//!                    |                                   |
//!                    +--> IN meter (raw input)          +--> OUT meter (processed output)
//! ```

#![cfg_attr(not(feature = "standalone"), allow(dead_code))]

#[cfg(feature = "standalone")]
mod standalone {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, Sample, SizedSample};
    use nice_plug::params::InternalParamMut;
    use nice_plug::prelude::*;
    use prevocal::{PreVocalDsp, PreVocalParams};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    slint::include_modules!();

    // ---------------------------------------------------------------------
    // Parameter bridge. The Slint UI talks to the same `Arc<PreVocalParams>`
    // the audio thread reads from, through raw [`ParamPtr`]s.
    // ---------------------------------------------------------------------

    struct UiBridge {
        params: Arc<PreVocalParams>,
    }

    impl UiBridge {
        fn new(params: &Arc<PreVocalParams>) -> Self {
            Self {
                params: Arc::clone(params),
            }
        }

        // The UI works in dB/Hz, the params store linear gain for drive/trim.
        fn write_drive(&self, db: f32) {
            let db = db.clamp(0.0, 24.0);
            if !db.is_finite() {
                return;
            }
            let gain = util::db_to_gain(db);
            unsafe {
                self.params.drive._internal_set_plain_value(gain);
            }
        }

        fn write_hpf(&self, hz: f32) {
            let hz = hz.clamp(20.0, 200.0);
            if !hz.is_finite() {
                return;
            }
            unsafe {
                self.params.hpf._internal_set_plain_value(hz);
            }
        }

        fn write_air(&self, db: f32) {
            let db = db.clamp(0.0, 6.0);
            if !db.is_finite() {
                return;
            }
            unsafe {
                self.params.air._internal_set_plain_value(db);
            }
        }

        fn write_phase(&self, enabled: bool) {
            unsafe {
                self.params.phase_flip._internal_set_plain_value(enabled);
            }
        }

        fn write_output_trim(&self, db: f32) {
            let db = db.clamp(-12.0, 12.0);
            if !db.is_finite() {
                return;
            }
            let gain = util::db_to_gain(db);
            unsafe {
                self.params.output_trim._internal_set_plain_value(gain);
            }
        }

        fn drive_value(&self) -> f32 {
            util::gain_to_db(self.params.drive.modulated_plain_value())
        }

        fn hpf_value(&self) -> f32 {
            self.params.hpf.modulated_plain_value()
        }

        fn air_value(&self) -> f32 {
            self.params.air.modulated_plain_value()
        }

        fn phase_value(&self) -> bool {
            self.params.phase_flip.modulated_plain_value()
        }

        fn output_trim_value(&self) -> f32 {
            util::gain_to_db(self.params.output_trim.modulated_plain_value())
        }
    }

    // ---------------------------------------------------------------------
    // A small ring buffer bridging the cpal input and output callbacks, which
    // run on independent schedules. Input writes processed frames; the output
    // drains them (playing silence when empty).
    // ---------------------------------------------------------------------

    struct FrameRing {
        data: Vec<f32>,
        read_idx: usize,
        write_idx: usize,
        frames: usize,
        channels: usize,
    }

    impl FrameRing {
        fn new(capacity_frames: usize, channels: usize) -> Self {
            Self {
                data: vec![0.0; capacity_frames * channels],
                read_idx: 0,
                write_idx: 0,
                frames: 0,
                channels,
            }
        }

        fn write(&mut self, interleaved: &[f32]) {
            let cap = self.capacity_frames();
            let num_frames = interleaved.len() / self.channels;
            let excess = (self.frames + num_frames).saturating_sub(cap);
            if excess > 0 {
                self.read_idx = (self.read_idx + excess * self.channels) % self.data.len();
                self.frames -= excess;
            }
            for &sample in interleaved {
                self.data[self.write_idx] = sample;
                self.write_idx = (self.write_idx + 1) % self.data.len();
            }
            self.frames += num_frames;
        }

        /// Copy up to `out.len()` samples (in frames) into `out`, returning the
        /// number of frames written.
        fn read(&mut self, out: &mut [f32]) -> usize {
            let want_frames = out.len() / self.channels;
            let take = self.frames.min(want_frames);
            for sample in out.iter_mut().take(take * self.channels) {
                *sample = self.data[self.read_idx];
                self.read_idx = (self.read_idx + 1) % self.data.len();
            }
            self.frames -= take;
            take
        }

        fn capacity_frames(&self) -> usize {
            self.data.len() / self.channels
        }
    }

    // ---------------------------------------------------------------------
    // Audio engine running the realtime DSP on the cpal callback thread.
    // It operates in the output channel layout (mono input gets duplicated,
    // excess input channels get folded down).
    // ---------------------------------------------------------------------

    struct AudioEngine {
        _params: Arc<PreVocalParams>,
        dsp: PreVocalDsp,
        _sample_rate: f32,
        num_channels: usize,
        work: Vec<Vec<f32>>,
    }

    impl AudioEngine {
        fn new(params: Arc<PreVocalParams>, sample_rate: f32, num_channels: usize) -> Self {
            let mut dsp = PreVocalDsp::new(params.clone());
            dsp.set_sample_rate(sample_rate);
            dsp.resize(num_channels);
            Self {
                _params: params,
                dsp,
                _sample_rate: sample_rate,
                num_channels,
                work: vec![Vec::new(); num_channels],
            }
        }

        fn process_block(&mut self, input: &[f32], output: &mut [f32]) {
            let num_frames = output.len() / self.num_channels;
            for channel_work in self.work.iter_mut() {
                channel_work.clear();
                channel_work.reserve(num_frames);
            }
            for frame in 0..num_frames {
                for channel in 0..self.num_channels {
                    self.work[channel].push(input[frame * self.num_channels + channel]);
                }
            }

            let mut slices: Vec<&mut [f32]> =
                self.work.iter_mut().map(|w| w.as_mut_slice()).collect();
            self.dsp.process_block(&mut slices);

            for frame in 0..num_frames {
                for channel in 0..self.num_channels {
                    output[frame * self.num_channels + channel] = self.work[channel][frame];
                }
            }
        }
    }

    // ---------------------------------------------------------------------
    // cpal stream builders. Input and output use their own device's sample
    // format, and both streams are returned so the caller can keep them alive
    // for the whole application lifetime (a dropped `Stream` stops playback).
    // ---------------------------------------------------------------------

    fn build_input_stream<T>(
        input_device: &cpal::Device,
        input_config: &cpal::StreamConfig,
        engine: Arc<Mutex<AudioEngine>>,
        input_channels: usize,
        engine_channels: usize,
        shared_out: Arc<Mutex<FrameRing>>,
        active: Arc<AtomicBool>,
        input_level: Arc<Mutex<f32>>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample + FromSample<f32>,
        f32: FromSample<T>,
    {
        input_device.build_input_stream(
            input_config.clone(),
            move |data: &[T], _| {
                if !active.load(Ordering::Relaxed) {
                    return;
                }

                // Map the device's channel layout onto the engine layout, duplicating
                // a mono input to stereo (or folding down excess channels).
                let frames = data.len() / input_channels;
                let mut interleaved = vec![0.0f32; frames * engine_channels];
                for frame in 0..frames {
                    for ch in 0..engine_channels {
                        let in_ch = if input_channels == 1 {
                            0
                        } else {
                            ch.min(input_channels - 1)
                        };
                        interleaved[frame * engine_channels + ch] =
                            data[frame * input_channels + in_ch].to_sample::<f32>();
                    }
                }

                // IN meter: RMS of the raw, pre-DSP input signal.
                let mut sum_sq = 0.0f32;
                for &s in interleaved.iter() {
                    sum_sq += s * s;
                }
                let rms = if interleaved.is_empty() {
                    0.0
                } else {
                    (sum_sq / interleaved.len() as f32).sqrt()
                };
                if let Ok(mut lvl) = input_level.lock() {
                    *lvl = rms.clamp(0.0, 1.0);
                }

                let mut processed = vec![0.0f32; interleaved.len()];
                engine.lock().unwrap().process_block(&interleaved, &mut processed);
                shared_out.lock().unwrap().write(&processed);
            },
            |err| tracing::error!("Input stream error: {err}"),
            None,
        )
    }

    fn build_output_stream<T>(
        output_device: &cpal::Device,
        output_config: &cpal::StreamConfig,
        shared_out: Arc<Mutex<FrameRing>>,
        active: Arc<AtomicBool>,
        output_level: Arc<Mutex<f32>>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample + FromSample<f32>,
        f32: FromSample<T>,
    {
        let output_channels = output_config.channels as usize;
        output_device.build_output_stream(
            output_config.clone(),
            move |data: &mut [T], _| {
                if !active.load(Ordering::Relaxed) {
                    for sample in data.iter_mut() {
                        *sample = 0.0f32.to_sample::<T>();
                    }
                    return;
                }

                let mut buffer = vec![0.0f32; data.len()];
                let filled_frames = shared_out.lock().unwrap().read(&mut buffer);
                let filled_samples = filled_frames * output_channels;

                // OUT meter: RMS of the processed signal actually going to the speakers.
                let mut sum_sq = 0.0f32;
                for &v in buffer[..filled_samples].iter() {
                    sum_sq += v * v;
                }
                let rms = if filled_samples == 0 {
                    0.0
                } else {
                    (sum_sq / filled_samples as f32).sqrt()
                };
                if let Ok(mut lvl) = output_level.lock() {
                    *lvl = rms.clamp(0.0, 1.0);
                }

                for (i, sample) in data.iter_mut().enumerate() {
                    *sample = if i < filled_samples {
                        buffer[i].to_sample::<T>()
                    } else {
                        0.0f32.to_sample::<T>()
                    };
                }
            },
            |err| tracing::error!("Output stream error: {err}"),
            None,
        )
    }

    fn build_input_stream_by_format(
        input_device: &cpal::Device,
        input_config: &cpal::StreamConfig,
        format: cpal::SampleFormat,
        engine: Arc<Mutex<AudioEngine>>,
        input_channels: usize,
        engine_channels: usize,
        shared_out: Arc<Mutex<FrameRing>>,
        active: Arc<AtomicBool>,
        input_level: Arc<Mutex<f32>>,
    ) -> Result<cpal::Stream, String> {
        macro_rules! build_input_streams {
            ($($format:path => $ty:ty),+ $(,)?) => {
                match format {
                    $(
                        $format => build_input_stream::<$ty>(
                            input_device,
                            input_config,
                            engine.clone(),
                            input_channels,
                            engine_channels,
                            shared_out.clone(),
                            active.clone(),
                            input_level.clone(),
                        )
                        .map_err(|e| format!("Could not build input stream: {e}")),
                    )+
                    other => Err(format!("Unsupported input sample format: {other:?}")),
                }
            };
        }
        build_input_streams!(
            cpal::SampleFormat::I8 => i8,
            cpal::SampleFormat::I16 => i16,
            cpal::SampleFormat::I32 => i32,
            cpal::SampleFormat::I64 => i64,
            cpal::SampleFormat::U8 => u8,
            cpal::SampleFormat::U16 => u16,
            cpal::SampleFormat::U32 => u32,
            cpal::SampleFormat::U64 => u64,
            cpal::SampleFormat::F32 => f32,
            cpal::SampleFormat::F64 => f64,
        )
    }

    fn build_output_stream_by_format(
        output_device: &cpal::Device,
        output_config: &cpal::StreamConfig,
        format: cpal::SampleFormat,
        shared_out: Arc<Mutex<FrameRing>>,
        active: Arc<AtomicBool>,
        output_level: Arc<Mutex<f32>>,
    ) -> Result<cpal::Stream, String> {
        macro_rules! build_output_streams {
            ($($format:path => $ty:ty),+ $(,)?) => {
                match format {
                    $(
                        $format => build_output_stream::<$ty>(
                            output_device,
                            output_config,
                            shared_out.clone(),
                            active.clone(),
                            output_level.clone(),
                        )
                        .map_err(|e| format!("Could not build output stream: {e}")),
                    )+
                    other => Err(format!("Unsupported output sample format: {other:?}")),
                }
            };
        }
        build_output_streams!(
            cpal::SampleFormat::I8 => i8,
            cpal::SampleFormat::I16 => i16,
            cpal::SampleFormat::I32 => i32,
            cpal::SampleFormat::I64 => i64,
            cpal::SampleFormat::U8 => u8,
            cpal::SampleFormat::U16 => u16,
            cpal::SampleFormat::U32 => u32,
            cpal::SampleFormat::U64 => u64,
            cpal::SampleFormat::F32 => f32,
            cpal::SampleFormat::F64 => f64,
        )
    }

    /// Pick an input config at the output's sample rate (preferring f32), falling
    /// back to the device's default config if nothing matches.
    fn pick_input_config(
        input_device: &cpal::Device,
        requested_rate: cpal::SampleRate,
    ) -> Result<(cpal::StreamConfig, cpal::SampleFormat), String> {
        let configs: Vec<_> = input_device
            .supported_input_configs()
            .map_err(|e| format!("Could not query input configs: {e}"))?
            .collect();
        let at_rate = configs
            .iter()
            .filter(|c| c.min_sample_rate() <= requested_rate && c.max_sample_rate() >= requested_rate)
            .collect::<Vec<_>>();
        let chosen = at_rate
            .iter()
            .find(|c| c.sample_format() == cpal::SampleFormat::F32)
            .or_else(|| at_rate.first())
            .copied();
        if let Some(cfg) = chosen {
            return Ok((cfg.with_sample_rate(requested_rate).config(), cfg.sample_format()));
        }

        tracing::warn!("No input config matches the output sample rate; using the input device default.");
        let default = input_device
            .default_input_config()
            .map_err(|e| format!("Could not query default input config: {e}"))?;
        Ok((default.config(), default.sample_format()))
    }

    /// Set up audio using the system's default input and output devices. Returns
    /// the streams so the caller can keep them alive for the whole app lifetime.
    fn setup_audio(
        params: Arc<PreVocalParams>,
    ) -> Result<
        (
            Vec<cpal::Stream>,
            Arc<Mutex<AudioEngine>>,
            Arc<AtomicBool>,
            Arc<Mutex<f32>>,
            Arc<Mutex<f32>>,
            Arc<Mutex<FrameRing>>,
        ),
        String,
    > {
        let host = cpal::default_host();
        let input_device = host
            .default_input_device()
            .ok_or_else(|| "No default audio input device found.".to_string())?;
        let output_device = host
            .default_output_device()
            .ok_or_else(|| "No default audio output device found.".to_string())?;

        let output_default = output_device
            .default_output_config()
            .map_err(|e| format!("Could not query default output config: {e}"))?;
        let sample_rate = output_default.sample_rate();
        let output_channels = output_default.channels() as usize;
        let output_sample_format = output_default.sample_format();
        let output_config = output_default.config();

        let (input_config, input_sample_format) = pick_input_config(&input_device, sample_rate)?;
        let input_channels = input_config.channels as usize;

        if input_channels != output_channels {
            tracing::warn!(
                "Input ({input_channels} ch) and output ({output_channels} ch) differ; mapping to the output layout."
            );
        }

        let engine = Arc::new(Mutex::new(AudioEngine::new(
            params,
            sample_rate as f32,
            output_channels,
        )));
        let shared_out = Arc::new(Mutex::new(FrameRing::new(8192, output_channels)));
        let active = Arc::new(AtomicBool::new(true));
        let input_level = Arc::new(Mutex::new(0.0f32));
        let output_level = Arc::new(Mutex::new(0.0f32));

        let input_stream = build_input_stream_by_format(
            &input_device,
            &input_config,
            input_sample_format,
            engine.clone(),
            input_channels,
            output_channels,
            shared_out.clone(),
            active.clone(),
            input_level.clone(),
        )?;

        let output_stream = build_output_stream_by_format(
            &output_device,
            &output_config,
            output_sample_format,
            shared_out.clone(),
            active.clone(),
            output_level.clone(),
        )?;

        input_stream
            .play()
            .map_err(|e| format!("Could not start input stream: {e}"))?;
        output_stream
            .play()
            .map_err(|e| format!("Could not start output stream: {e}"))?;

        Ok((
            vec![input_stream, output_stream],
            engine,
            active,
            input_level,
            output_level,
            shared_out,
        ))
    }

    // ---------------------------------------------------------------------
    // Slint UI setup + event loop.
    // ---------------------------------------------------------------------

    fn run_gui(
        bridge: Arc<UiBridge>,
        engine: Arc<Mutex<AudioEngine>>,
        shared_out: Arc<Mutex<FrameRing>>,
        active: Arc<AtomicBool>,
        input_level: Arc<Mutex<f32>>,
        output_level: Arc<Mutex<f32>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ui = PreVocalUI::new()?;

        ui.set_drive(bridge.drive_value());
        ui.set_hpf(bridge.hpf_value());
        ui.set_air(bridge.air_value());
        ui.set_output_trim(bridge.output_trim_value());
        ui.set_phase_flip(bridge.phase_value());

        let bridge_drive = Arc::clone(&bridge);
        ui.on_drive_changed(move |v| bridge_drive.write_drive(v));

        let bridge_hpf = Arc::clone(&bridge);
        ui.on_hpf_changed(move |v| bridge_hpf.write_hpf(v));

        let bridge_air = Arc::clone(&bridge);
        ui.on_air_changed(move |v| bridge_air.write_air(v));

        let bridge_trim = Arc::clone(&bridge);
        ui.on_output_trim_changed(move |v| bridge_trim.write_output_trim(v));

        let bridge_phase = Arc::clone(&bridge);
        ui.on_phase_flip_changed(move |v| bridge_phase.write_phase(v));

        let _ = (&engine, &shared_out);

        // Poll the level shared values and drive the UI meters with a little
        // ballistics smoothing so they don't flicker.
        let ui_weak = ui.as_weak();
        let input_poll = Arc::clone(&input_level);
        let output_poll = Arc::clone(&output_level);
        let active_poll = active.clone();
        std::thread::spawn(move || {
            use std::time::Duration;
            let mut smooth_in = 0.0f32;
            let mut smooth_out = 0.0f32;
            while active_poll.load(Ordering::Relaxed) {
                let in_lvl = *input_poll.lock().unwrap();
                let out_lvl = *output_poll.lock().unwrap();
                smooth_in += (in_lvl - smooth_in) * 0.45;
                smooth_out += (out_lvl - smooth_out) * 0.45;
                let _ = ui_weak.upgrade_in_event_loop(move |ui| ui.set_input_level(smooth_in));
                let _ = ui_weak.upgrade_in_event_loop(move |ui| ui.set_output_level(smooth_out));
                std::thread::sleep(Duration::from_millis(40));
            }
        });

        ui.run()?;
        Ok(())
    }

    pub fn run() {
        let params = Arc::new(PreVocalParams::default());
        let bridge = UiBridge::new(&params);

        // The streams must stay alive while the GUI runs, otherwise cpal stops the
        // audio as soon as they are dropped.
        let (streams, engine, active, input_level, output_level, shared_out) =
            match setup_audio(params) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            };

        if let Err(e) = run_gui(
            bridge.into(),
            engine,
            shared_out,
            active,
            input_level,
            output_level,
        ) {
            eprintln!("GUI error: {e}");
        }

        // Explicitly stop audio before exiting.
        for stream in &streams {
            let _ = stream.pause();
        }
        drop(streams);
    }
}

fn main() {
    #[cfg(feature = "standalone")]
    standalone::run();

    #[cfg(not(feature = "standalone"))]
    {
        eprintln!("Standalone mode is disabled. Build with `cargo run --features standalone --bin prevocal-standalone`.");
    }
}
