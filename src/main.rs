//! PreVocal standalone binary.
//!
//! This target runs the same DSP as the plugin as a native Linux application using Slint for
//! the GUI and cpal for realtime audio I/O, so it can be tested without a DAW. The DSP lives
//! in `prevocal::PreVocalDsp` and is shared with the plugin `lib.rs`.

#![cfg_attr(not(feature = "standalone"), allow(dead_code))]

#[cfg(feature = "standalone")]
mod standalone {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, Sample};
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
    // cpal stream setup, generic over the device sample format.
    // ---------------------------------------------------------------------

    fn run_stream<T>(
        engine: Arc<Mutex<AudioEngine>>,
        shared_out: Arc<Mutex<FrameRing>>,
        active: Arc<AtomicBool>,
        shared_level: Arc<Mutex<f32>>,
    ) -> Result<(), cpal::Error>
    where
        T: cpal::SizedSample + FromSample<f32>,
        f32: FromSample<T>,
    {
        let host = cpal::default_host();
        let input_device = host
            .default_input_device()
            .ok_or_else(|| cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable))?;
        let output_device = host
            .default_output_device()
            .ok_or_else(|| cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable))?;

        let output_config = output_device.default_output_config()?;
        let sample_rate = output_config.sample_rate();
        let output_channels = output_config.channels() as usize;

        let input_config = match input_device.supported_input_configs() {
            Ok(configs) => configs
                .filter_map(|cfg| {
                    if cfg.min_sample_rate() <= sample_rate
                        && cfg.max_sample_rate() >= sample_rate
                    {
                        Some(cfg.with_sample_rate(sample_rate).config())
                    } else {
                        None
                    }
                })
                .min_by_key(|cfg| cfg.channels),
            Err(_) => None,
        }
        .unwrap_or_else(|| input_device.default_input_config().unwrap().config());

        if input_config.channels as usize != output_channels {
            tracing::warn!(
                "Input ({} ch) and output ({} ch) channel counts differ.",
                input_config.channels,
                output_channels
            );
        }

        let input_engine = engine.clone();
        let input_shared = shared_out.clone();
        let input_active = active.clone();
        let input_level = shared_level.clone();

        let input_stream = input_device.build_input_stream::<T, _, _>(
            input_config,
            move |data: &[T], _| {
                if !input_active.load(Ordering::Relaxed) {
                    return;
                }
                let mut engine = input_engine.lock().unwrap();
                let interleaved: Vec<f32> = data.iter().map(|s| s.to_sample::<f32>()).collect();
                let mut processed = vec![0.0f32; interleaved.len()];
                engine.process_block(&interleaved, &mut processed);
                drop(engine);
                // compute RMS across processed samples
                let mut sum_sq = 0.0f32;
                for &s in processed.iter() {
                    sum_sq += s * s;
                }
                let rms = if processed.is_empty() { 0.0 } else { (sum_sq / processed.len() as f32).sqrt() };
                if let Ok(mut lvl) = input_level.lock() {
                    *lvl = rms.clamp(0.0, 1.0);
                }
                input_shared.lock().unwrap().write(&processed);
            },
            |err| tracing::error!("Input stream error: {err}"),
            None,
        )?;

        let output_shared = shared_out.clone();

        let output_stream = output_device.build_output_stream::<T, _, _>(
            output_config.config(),
            move |data: &mut [T], _| {
                let mut ring = output_shared.lock().unwrap();
                let mut mono = vec![0.0f32; data.len()];
                let filled_frames = ring.read(&mut mono);
                let filled = filled_frames * output_channels;
                for (i, sample) in data.iter_mut().enumerate() {
                    *sample = if i < filled {
                        mono[i].to_sample::<T>()
                    } else {
                        0.0f32.to_sample::<T>()
                    };
                }
            },
            |err| tracing::error!("Output stream error: {err}"),
            None,
        )?;

        input_stream.play()?;
        output_stream.play()?;

        Ok(())
    }

    fn run_audio(
        params: Arc<PreVocalParams>,
    ) -> Result<(Arc<AtomicBool>, Arc<Mutex<FrameRing>>, Arc<Mutex<AudioEngine>>, Arc<Mutex<f32>>), String> {
        let host = cpal::default_host();
        let output_device = host
            .default_output_device()
            .ok_or_else(|| "No default audio output device found.".to_string())?;
        let output_config = output_device
            .default_output_config()
            .map_err(|e| format!("Could not query default output config: {e}"))?;

        let sample_rate = output_config.sample_rate() as f32;
        let num_channels = output_config.channels() as usize;

        let engine = Arc::new(Mutex::new(AudioEngine::new(
            params,
            sample_rate,
            num_channels,
        )));
        let shared_out = Arc::new(Mutex::new(FrameRing::new(8192, num_channels)));
        let active = Arc::new(AtomicBool::new(true));
        let shared_level = Arc::new(Mutex::new(0.0f32));

        let dispatch = move |engine: Arc<Mutex<AudioEngine>>,
                             shared_out: Arc<Mutex<FrameRing>>,
                             active: Arc<AtomicBool>|
              -> Result<(), cpal::Error> {
            match output_config.sample_format() {
                cpal::SampleFormat::F32 => run_stream::<f32>(engine, shared_out, active, shared_level.clone()),
                cpal::SampleFormat::F64 => run_stream::<f64>(engine, shared_out, active, shared_level.clone()),
                cpal::SampleFormat::I16 => run_stream::<i16>(engine, shared_out, active, shared_level.clone()),
                cpal::SampleFormat::U16 => run_stream::<u16>(engine, shared_out, active, shared_level.clone()),
                cpal::SampleFormat::I32 => run_stream::<i32>(engine, shared_out, active, shared_level.clone()),
                cpal::SampleFormat::U32 => run_stream::<u32>(engine, shared_out, active, shared_level.clone()),
                cpal::SampleFormat::I8 => run_stream::<i8>(engine, shared_out, active, shared_level.clone()),
                cpal::SampleFormat::U8 => run_stream::<u8>(engine, shared_out, active, shared_level.clone()),
                other => {
                    return Err(cpal::Error::with_message(
                        cpal::ErrorKind::UnsupportedConfig,
                        format!("Unsupported output sample format: {other:?}"),
                    ))
                }
            }
        };

        match dispatch(engine.clone(), shared_out.clone(), active.clone()) {
            Ok(()) => Ok((active, shared_out, engine, shared_level)),
            Err(e) => Err(format!("Could not build audio streams: {e}")),
        }
    }

    // ---------------------------------------------------------------------
    // Slint UI setup + event loop.
    // ---------------------------------------------------------------------

    fn run_gui(
        bridge: Arc<UiBridge>,
        engine: Arc<Mutex<AudioEngine>>,
        shared_out: Arc<Mutex<FrameRing>>,
        active: Arc<AtomicBool>,
        shared_level: Arc<Mutex<f32>>,
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

        let _ = (engine, shared_out, active);

        // spawn a thread to poll shared_level and update the UI meter
        let ui_weak = ui.as_weak();
        let level_poll = Arc::clone(&shared_level);
        let active_poll = active.clone();
        std::thread::spawn(move || {
            use std::time::Duration;
            while active_poll.load(Ordering::Relaxed) {
                let lvl = *level_poll.lock().unwrap();
                // update UI meter; Slint weak handle will ignore if UI closed
                ui_weak.set_input_level(lvl);
                std::thread::sleep(Duration::from_millis(60));
            }
        });

        ui.run()?;
        Ok(())
    }

    pub fn run() {
        let params = Arc::new(PreVocalParams::default());
        let bridge = UiBridge::new(&params);

        match run_audio(params) {
            Ok((active, shared_out, engine, shared_level)) => {
                if let Err(e) = run_gui(bridge.into(), engine, shared_out, active, shared_level) {
                    eprintln!("GUI error: {e}");
                }
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }
}

fn main() {
    #[cfg(feature = "standalone")]
    standalone::run();

    #[cfg(not(feature = "standalone"))]
    {
        eprintln!("Standalone mode is disabled. Build with `cargo run --features standalone --bin prevocal-standalone` after installing ALSA/JACK system libraries.");
    }
}
