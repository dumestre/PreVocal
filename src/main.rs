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
//! selected input device -> DSP -> ring buffer -> selected output device
//!            |                                    |
//!            +--> IN meter (raw input)           +--> OUT meter (processed output)
//! ```
//!
//! The toolbar lets you pick the audio driver/host and device (e.g. ASIO or WASAPI),
//! restart the engine and rescan the device list. The last selection is persisted to a
//! small file so it is restored on the next launch. When a device only supports input
//! (or only output), the other side falls back to the host's default device.

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

        /// Write interleaved frames into the ring. If the buffer would overflow, the
        /// *newest* frames are dropped (the oldest of the incoming block) instead of
        /// advancing `read_idx`; skipping ahead in the playback path would create a
        /// discontinuity (click) in the output signal.
        fn write(&mut self, interleaved: &[f32]) {
            let cap = self.capacity_frames();
            let num_frames = interleaved.len() / self.channels;
            let excess = (self.frames + num_frames).saturating_sub(cap);
            if excess > 0 {
                let skip = excess.min(num_frames);
                let start = skip * self.channels;
                for &sample in interleaved[start..].iter() {
                    self.data[self.write_idx] = sample;
                    self.write_idx = (self.write_idx + 1) % self.data.len();
                }
                self.frames += num_frames - skip;
                return;
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
    // Real-time DSP wrapper operating on the interleaved buffer directly,
    // so the cpal callbacks never allocate beyond a single Vec per block.
    // ---------------------------------------------------------------------

    struct AudioEngine {
        dsp: PreVocalDsp,
        num_channels: usize,
    }

    impl AudioEngine {
        fn new(params: Arc<PreVocalParams>, sample_rate: f32, num_channels: usize) -> Self {
            let mut dsp = PreVocalDsp::new(params);
            dsp.set_sample_rate(sample_rate);
            dsp.resize(num_channels);
            Self { dsp, num_channels }
        }

        fn process(&mut self, interleaved: &mut [f32]) {
            self.dsp.process_interleaved(interleaved, self.num_channels);
        }
    }

    // ---------------------------------------------------------------------
    // Meter state shared with the GUI poll thread.
    // ---------------------------------------------------------------------

    #[derive(Default)]
    struct MeterState {
        in_level: f32,
        in_peak: f32,
        out_level: f32,
        out_peak: f32,
    }

    // ---------------------------------------------------------------------
    // Meter scale. Levels are exposed to the UI as a 0..1 mapping of the
    // dBFS scale: -60 dB => 0.0, 0 dB => 1.0. The Slint meter colors the
    // zones green (< -6 dB), yellow (-6..-3 dB) and red (>= -3 dB clip).
    // ---------------------------------------------------------------------

    fn level_to_meter(rms: f32) -> f32 {
        if rms <= 0.0 {
            return 0.0;
        }
        let db = 20.0 * rms.log10();
        ((db + 60.0) / 60.0).clamp(0.0, 1.0)
    }

    // ---------------------------------------------------------------------
    // Audio manager: enumerates hosts + devices, keeps the selected device
    // alive and owns the cpal streams. The GUI talks to it through a Mutex.
    // ---------------------------------------------------------------------

    /// A single selectable host/device pair.
    #[derive(Clone)]
    struct DeviceEntry {
        host_id: cpal::HostId,
        device_name: String,
        device: cpal::Device,
    }

    struct AudioManager {
        params: Arc<PreVocalParams>,
        entries: Vec<DeviceEntry>,
        labels: Vec<String>,
        current: usize,
        /// Gates the audio callbacks (toggled on stop/start).
        active: Arc<AtomicBool>,
        /// Keeps the meter poll thread alive for the whole app lifetime.
        alive: Arc<AtomicBool>,
        meters: Arc<Mutex<MeterState>>,
        streams: Option<Vec<cpal::Stream>>,
        status: String,
    }

    impl AudioManager {
        fn new(params: Arc<PreVocalParams>) -> Self {
            let entries = Self::enumerate_entries();
            let labels = Self::make_labels(&entries);
            let mut mgr = Self {
                params,
                entries,
                labels,
                current: 0,
                active: Arc::new(AtomicBool::new(false)),
                alive: Arc::new(AtomicBool::new(true)),
                meters: Arc::new(Mutex::new(MeterState::default())),
                streams: None,
                status: String::new(),
            };

            // Restore the last used device if it's still present.
            if let Some((host_name, device_name)) = Self::load_last_selection()
                && let Some(idx) = mgr.entries.iter().position(|e| {
                    e.host_id.name().eq_ignore_ascii_case(&host_name)
                        && e.device_name.eq_ignore_ascii_case(&device_name)
                })
            {
                mgr.current = idx;
            }

            if let Err(e) = mgr.start() {
                tracing::error!("Audio start failed: {e}");
            }
            mgr
        }

        fn enumerate_entries() -> Vec<DeviceEntry> {
            let mut entries = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for host_id in cpal::available_hosts() {
                let Ok(host) = cpal::host_from_id(host_id) else {
                    continue;
                };
                let mut add = |device: cpal::Device| {
                    let name = device.to_string();
                    if seen.insert((host_id, name.clone())) {
                        entries.push(DeviceEntry {
                            host_id,
                            device_name: name,
                            device,
                        });
                    }
                };
                if let Ok(devices) = host.devices() {
                    for device in devices {
                        add(device);
                    }
                }
                if let Some(device) = host.default_input_device() {
                    add(device);
                }
                if let Some(device) = host.default_output_device() {
                    add(device);
                }
            }
            entries
        }

        fn make_labels(entries: &[DeviceEntry]) -> Vec<String> {
            entries
                .iter()
                .map(|e| format!("{} — {}", e.host_id.name(), e.device_name))
                .collect()
        }

        // Persisted selection (host name + device name), stored next to the executable.
        fn settings_path() -> std::path::PathBuf {
            std::env::current_dir()
                .unwrap_or_default()
                .join("prevocal-last-device.txt")
        }

        fn load_last_selection() -> Option<(String, String)> {
            let text = std::fs::read_to_string(Self::settings_path()).ok()?;
            let mut lines = text.lines();
            let host = lines.next()?.trim().to_string();
            let device = lines.next()?.trim().to_string();
            if host.is_empty() || device.is_empty() {
                return None;
            }
            Some((host, device))
        }

        fn save_last_selection(&self) {
            if let Some(entry) = self.entries.get(self.current) {
                let _ = std::fs::write(
                    Self::settings_path(),
                    format!("{}\n{}\n", entry.host_id.name(), entry.device_name),
                );
            }
        }

        fn stop(&mut self) {
            self.active.store(false, Ordering::Relaxed);
            self.streams.take();
        }

        fn start(&mut self) -> Result<(), String> {
            self.stop();
            let result = self.try_start();
            match &result {
                Ok(status) => self.status = status.clone(),
                Err(e) => self.status = format!("ERROR: {e}"),
            }
            result.map(|_| ())
        }

        fn try_start(&mut self) -> Result<String, String> {
            let entry = self
                .entries
                .get(self.current)
                .cloned()
                .ok_or_else(|| "No audio device selected.".to_string())?;
            let host = cpal::host_from_id(entry.host_id)
                .map_err(|e| format!("Could not load host '{}': {e}", entry.host_id.name()))?;

            // Use the selected device for whichever side it supports; fall back to the
            // host's default for the other side (e.g. a mic-only or speaker-only device).
            let input_device = if entry.device.supports_input() {
                entry.device.clone()
            } else {
                host.default_input_device().ok_or_else(|| {
                    format!("No input device available for host '{}'.", entry.host_id.name())
                })?
            };
            let output_device = if entry.device.supports_output() {
                entry.device.clone()
            } else {
                host.default_output_device().ok_or_else(|| {
                    format!("No output device available for host '{}'.", entry.host_id.name())
                })?
            };

            let output_default = output_device
                .default_output_config()
                .map_err(|e| format!("Could not query output config for '{output_device}': {e}"))?;
            let sample_rate = output_default.sample_rate();
            let output_channels = output_default.channels() as usize;
            let output_sample_format = output_default.sample_format();
            let output_config = output_default.config();

            let (input_config, input_sample_format) = pick_input_config(&input_device, sample_rate)?;
            let input_channels = input_config.channels as usize;

            let engine = Arc::new(Mutex::new(AudioEngine::new(
                self.params.clone(),
                sample_rate as f32,
                output_channels,
            )));
            // Small ring: roughly ~20 ms at 48 kHz (was 8192 frames ≈ 170 ms). The
            // underrun/overflow handling keeps it glitch-free, so latency stays low.
            let ring_capacity = (sample_rate as usize / 50).clamp(512, 4096);
            let shared_out = Arc::new(Mutex::new(FrameRing::new(ring_capacity, output_channels)));
            let active = self.active.clone();
            active.store(true, Ordering::Relaxed);

            let input_stream = build_input_stream_by_format(
                &input_device,
                &input_config,
                input_sample_format,
                engine.clone(),
                input_channels,
                output_channels,
                shared_out.clone(),
                active.clone(),
                self.meters.clone(),
            )?;

            let output_stream = build_output_stream_by_format(
                &output_device,
                &output_config,
                output_sample_format,
                shared_out.clone(),
                active.clone(),
                self.meters.clone(),
            )?;

            // Start the output stream first so it consumes the ring while it is still
            // empty. Starting the input first would fill the whole ring (~170 ms of
            // buffered audio) before any playback begins, adding noticeable latency.
            output_stream
                .play()
                .map_err(|e| format!("Could not start output stream: {e}"))?;
            input_stream
                .play()
                .map_err(|e| format!("Could not start input stream: {e}"))?;

            self.streams = Some(vec![input_stream, output_stream]);
            self.save_last_selection();

            Ok(format!(
                "RUNNING — {} ({} Hz, {} in / {} out ch)",
                entry.device_name, sample_rate, input_channels, output_channels
            ))
        }

        fn select(&mut self, idx: usize) {
            if idx >= self.entries.len() || idx == self.current {
                return;
            }
            self.current = idx;
            if let Err(e) = self.start() {
                tracing::error!("Audio restart failed: {e}");
            }
        }

        /// Select a device by its ComboBox label ("HOST — DEVICE").
        fn select_by_label(&mut self, label: &str) {
            if let Some(idx) = self.labels.iter().position(|l| l == label) {
                self.select(idx);
            }
        }

        fn refresh(&mut self) {
            let previous = self
                .entries
                .get(self.current)
                .map(|e| (e.host_id, e.device_name.clone()));
            self.stop();
            self.entries = Self::enumerate_entries();
            self.labels = Self::make_labels(&self.entries);

            if let Some((host_id, name)) = previous {
                let found = self
                    .entries
                    .iter()
                    .position(|e| e.host_id == host_id && e.device_name == name)
                    .or_else(|| self.entries.iter().position(|e| e.host_id == host_id))
                    .unwrap_or(0);
                self.current = found;
            } else {
                self.current = 0;
            }

            if self.entries.is_empty() {
                self.status = "ERROR: no audio devices found".to_string();
                return;
            }
            if let Err(e) = self.start() {
                tracing::error!("Audio restart failed: {e}");
            }
        }

        fn shutdown(&mut self) {
            self.alive.store(false, Ordering::Relaxed);
            self.stop();
        }

        fn device_index(&self) -> i32 {
            self.current as i32
        }

        fn driver_name(&self) -> String {
            self.entries
                .get(self.current)
                .map(|e| e.host_id.name().to_string())
                .unwrap_or_default()
        }

        fn is_running(&self) -> bool {
            self.status.starts_with("RUNNING")
        }

        fn status(&self) -> String {
            self.status.clone()
        }
    }

    // ---------------------------------------------------------------------
    // cpal stream builders. Input and output use their own device's sample
    // format, and both streams are returned so the caller can keep them alive
    // for the whole application lifetime (a dropped `Stream` stops playback).
    // ---------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn build_input_stream<T>(
        input_device: &cpal::Device,
        input_config: &cpal::StreamConfig,
        engine: Arc<Mutex<AudioEngine>>,
        input_channels: usize,
        engine_channels: usize,
        shared_out: Arc<Mutex<FrameRing>>,
        active: Arc<AtomicBool>,
        meters: Arc<Mutex<MeterState>>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample + FromSample<f32>,
        f32: FromSample<T>,
    {
        // Reused buffer so the realtime callback never allocates.
        let mut interleaved: Vec<f32> = Vec::new();
        input_device.build_input_stream(
            *input_config,
            move |data: &[T], _| {
                if !active.load(Ordering::Relaxed) {
                    return;
                }

                // Map the device's channel layout onto the engine layout. A vocal preamp
                // is inherently mono-centric: the input channels are downmixed to a single
                // mono signal and duplicated across every engine channel, so a mono mic
                // (or a mic on just the left side of a stereo interface) reaches both
                // output channels.
                let frames = data.len() / input_channels;
                interleaved.resize(frames * engine_channels, 0.0);
                for frame in 0..frames {
                    let mut mono = 0.0f32;
                    for in_ch in 0..input_channels {
                        mono += data[frame * input_channels + in_ch].to_sample::<f32>();
                    }
                    mono /= input_channels as f32;
                    for out_ch in 0..engine_channels {
                        interleaved[frame * engine_channels + out_ch] = mono;
                    }
                }

                // IN meter: RMS + peak of the raw, pre-DSP input signal.
                let mut sum_sq = 0.0f32;
                let mut peak = 0.0f32;
                for &s in interleaved.iter() {
                    sum_sq += s * s;
                    let a = s.abs();
                    if a > peak {
                        peak = a;
                    }
                }
                let rms = if interleaved.is_empty() {
                    0.0
                } else {
                    (sum_sq / interleaved.len() as f32).sqrt()
                };
                if let Ok(mut m) = meters.lock() {
                    m.in_level = level_to_meter(rms);
                    m.in_peak = peak;
                }

                engine.lock().unwrap().process(&mut interleaved);
                shared_out.lock().unwrap().write(&interleaved);
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
        meters: Arc<Mutex<MeterState>>,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample + FromSample<f32>,
        f32: FromSample<T>,
    {
        let output_channels = output_config.channels as usize;
        // Reused buffers so the realtime callback never allocates.
        let mut buffer: Vec<f32> = Vec::new();
        let mut last_sample = 0.0f32;
        output_device.build_output_stream(
            *output_config,
            move |data: &mut [T], _| {
                if !active.load(Ordering::Relaxed) {
                    last_sample = 0.0;
                    for sample in data.iter_mut() {
                        *sample = 0.0f32.to_sample::<T>();
                    }
                    return;
                }

                buffer.resize(data.len(), 0.0);
                let filled_frames = shared_out.lock().unwrap().read(&mut buffer);
                let filled_samples = filled_frames * output_channels;

                // OUT meter: RMS + peak of the processed signal going to the speakers.
                let mut sum_sq = 0.0f32;
                let mut peak = 0.0f32;
                for &v in buffer[..filled_samples].iter() {
                    sum_sq += v * v;
                    let a = v.abs();
                    if a > peak {
                        peak = a;
                    }
                }
                let rms = if filled_samples == 0 {
                    0.0
                } else {
                    (sum_sq / filled_samples as f32).sqrt()
                };
                if let Ok(mut m) = meters.lock() {
                    m.out_level = level_to_meter(rms);
                    m.out_peak = peak;
                }

                // On underrun, hold the last valid sample instead of writing zeros;
                // repeating a constant avoids the hard discontinuity (click) that
                // silence gaps produce when the audio resumes.
                let mut held = last_sample;
                for (i, sample) in data.iter_mut().enumerate() {
                    let v = if i < filled_samples {
                        buffer[i]
                    } else {
                        held
                    };
                    held = v;
                    *sample = v.to_sample::<T>();
                }
                last_sample = held;
            },
            |err| tracing::error!("Output stream error: {err}"),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_input_stream_by_format(
        input_device: &cpal::Device,
        input_config: &cpal::StreamConfig,
        format: cpal::SampleFormat,
        engine: Arc<Mutex<AudioEngine>>,
        input_channels: usize,
        engine_channels: usize,
        shared_out: Arc<Mutex<FrameRing>>,
        active: Arc<AtomicBool>,
        meters: Arc<Mutex<MeterState>>,
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
                            meters.clone(),
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
        meters: Arc<Mutex<MeterState>>,
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
                            meters.clone(),
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

    // ---------------------------------------------------------------------
    // Slint UI setup + event loop.
    // ---------------------------------------------------------------------

    fn string_model(v: &[String]) -> slint::ModelRc<slint::SharedString> {
        let items: Vec<slint::SharedString> = v.iter().map(|s| s.as_str().into()).collect();
        slint::ModelRc::new(slint::VecModel::from(items))
    }

    fn run_gui(
        ui: PreVocalUI,
        bridge: Arc<UiBridge>,
        manager: Arc<Mutex<AudioManager>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        ui.set_drive(bridge.drive_value());
        ui.set_hpf(bridge.hpf_value());
        ui.set_air(bridge.air_value());
        ui.set_output_trim(bridge.output_trim_value());
        ui.set_phase_flip(bridge.phase_value());

        {
            let mgr = manager.lock().unwrap();
            ui.set_available_devices(string_model(&mgr.labels));
            ui.set_device_index(mgr.device_index());
            ui.set_driver_name(mgr.driver_name().into());
            ui.set_audio_status(mgr.status().into());
            ui.set_audio_running(mgr.is_running());
        }

        let ui_weak = ui.as_weak();

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

        // The user picked a device in the ComboBox: switch the audio engine to it.
        let mgr_select = Arc::clone(&manager);
        let weak_select = ui_weak.clone();
        ui.on_device_selected(move |label: slint::SharedString| {
            let mut mgr = mgr_select.lock().unwrap();
            mgr.select_by_label(label.as_str());
            if let Some(ui) = weak_select.upgrade() {
                ui.set_device_index(mgr.device_index());
                ui.set_driver_name(mgr.driver_name().into());
                ui.set_audio_status(mgr.status().into());
                ui.set_audio_running(mgr.is_running());
            }
        });

        // Rescan the host/device lists (e.g. after plugging in an interface).
        let mgr_refresh = Arc::clone(&manager);
        let weak_refresh = ui_weak.clone();
        ui.on_refresh_clicked(move || {
            let mut mgr = mgr_refresh.lock().unwrap();
            mgr.refresh();
            if let Some(ui) = weak_refresh.upgrade() {
                ui.set_available_devices(string_model(&mgr.labels));
                ui.set_device_index(mgr.device_index());
                ui.set_driver_name(mgr.driver_name().into());
                ui.set_audio_status(mgr.status().into());
                ui.set_audio_running(mgr.is_running());
            }
        });

        // Force a restart with the currently selected device (applies after errors).
        let mgr_restart = Arc::clone(&manager);
        let weak_restart = ui_weak.clone();
        ui.on_restart_clicked(move || {
            let mut mgr = mgr_restart.lock().unwrap();
            if let Err(e) = mgr.start() {
                tracing::error!("Audio restart failed: {e}");
            }
            if let Some(ui) = weak_restart.upgrade() {
                ui.set_device_index(mgr.device_index());
                ui.set_driver_name(mgr.driver_name().into());
                ui.set_audio_status(mgr.status().into());
                ui.set_audio_running(mgr.is_running());
            }
        });

        // Poll the meter values and drive the UI with a little ballistics smoothing.
        let meters = manager.lock().unwrap().meters.clone();
        let alive = manager.lock().unwrap().alive.clone();
        let weak_meter = ui_weak.clone();
        std::thread::spawn(move || {
            use std::time::Duration;
            let mut smooth_in = 0.0f32;
            let mut smooth_out = 0.0f32;
            let mut peak_in = 0.0f32;
            let mut peak_out = 0.0f32;
            while alive.load(Ordering::Relaxed) {
                let m = meters.lock().unwrap();
                let in_lvl = m.in_level;
                let in_pk = level_to_meter(m.in_peak);
                let out_lvl = m.out_level;
                let out_pk = level_to_meter(m.out_peak);
                drop(m);

                smooth_in += (in_lvl - smooth_in) * 0.45;
                smooth_out += (out_lvl - smooth_out) * 0.45;
                peak_in = peak_in.max(in_pk);
                peak_out = peak_out.max(out_pk);
                peak_in = (peak_in - 0.02).max(in_pk);
                peak_out = (peak_out - 0.02).max(out_pk);

                let _ = weak_meter.upgrade_in_event_loop(move |ui| ui.set_input_level(smooth_in));
                let _ = weak_meter.upgrade_in_event_loop(move |ui| ui.set_output_level(smooth_out));
                let _ = weak_meter.upgrade_in_event_loop(move |ui| ui.set_input_peak(peak_in));
                let _ = weak_meter.upgrade_in_event_loop(move |ui| ui.set_output_peak(peak_out));
                std::thread::sleep(Duration::from_millis(40));
            }
        });

        ui.run()?;
        Ok(())
    }

    pub fn run() {
        let params = Arc::new(PreVocalParams::default());
        let bridge = UiBridge::new(&params);

        // The manager owns the cpal streams, which must stay alive while the GUI
        // runs, otherwise cpal stops the audio as soon as they are dropped.
        let manager = Arc::new(Mutex::new(AudioManager::new(params)));

        let ui = match PreVocalUI::new() {
            Ok(ui) => ui,
            Err(e) => {
                eprintln!("GUI error: {e}");
                manager.lock().unwrap().shutdown();
                return;
            }
        };

        if let Err(e) = run_gui(ui, bridge.into(), manager.clone()) {
            eprintln!("GUI error: {e}");
        }

        manager.lock().unwrap().shutdown();
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
